// use nom::{Err, IResult, bytes::streaming::take, combinator::map_res, error::{Error, ErrorKind}, number::streaming::le_u32};
use thiserror::Error;
use std::{collections::HashMap, convert::TryInto, io, num::NonZeroUsize, str, time::Instant};
use crate::{mmap_vec::{VarMmapVec, VariableLength, WriteData}, types::{Bit, BitSlice, BitVec, Qit, QitSlice, SizeInBytes}};

// type IResult<I, O> = Result<(I, O), Err<()>>;

#[derive(Error, Debug)]
pub enum Reason {
    #[error("an invalid magic header was present")]
    InvalidMagic,
    #[error("an invalid version header was present")]
    InvalidVersion,
    #[error("storage id is not valid")]
    InvalidStorageId,
    #[error("storage id was already used")]
    DuplicatedStorageId,
    #[error("bytes are not valid utf-8")]
    InvalidUTF8,
    #[error("an invalid signedness value was present")]
    InvalidSignedValue,
    #[error("an invalid variable interpretation value was present")]
    InvalidInterpretationValue,
    #[error("an invalid storage type was present")]
    InvalidStorageType,
    #[error("an invalid varint was present")]
    InvalidVarInt,
    #[error(transparent)]
    IoError(#[from] io::Error),
    #[error("an invalid block type was present")]
    InvalidBlockType,
}

pub enum Error {
    Incomplete(Option<NonZeroUsize>),
    Failure(Reason),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Failure(Reason::IoError(e))
    }
}

type ParseResult<'i, T> = Result<(&'i [u8], T), Error>;

pub trait Parse<'i, Output = Self> {
    fn parse(i: &'i [u8]) -> ParseResult<'i, Output>;
}

pub trait ParseWith<'i, Extra, Output = Self> {
    fn parse_with(i: &'i [u8], extra: Extra) -> ParseResult<'i, Output>;
}

pub trait StorageLookup {
    fn lookup(&self, storage_id: u32) -> Option<&StorageDeclaration>;
}

fn take(count: usize) -> impl Fn(&[u8]) -> ParseResult<&[u8]> {
    // let count = count.into();
    move |i| {
        if i.len() < count {
            Err(Error::Incomplete(NonZeroUsize::new(count - i.len())))
        } else {
            let (taken, rest) = i.split_at(count);
            Ok((rest, taken))
        }
    }
}

impl<'i> Parse<'i> for u8 {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        if let Ok(bytes) = i.try_into() {
            Ok((&i[1..], u8::from_le_bytes(bytes)))
        } else {
            Err(Error::Incomplete(NonZeroUsize::new(1)))
        }
    }
}

impl<'i> Parse<'i> for u32 {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        if let Ok(bytes) = i.try_into() {
            Ok((&i[1..], u32::from_le_bytes(bytes)))
        } else {
            Err(Error::Incomplete(NonZeroUsize::new(4 - i.len())))
        }
    }
}

pub struct Varu32;
pub struct Varu64;

impl<'i> Parse<'i, u32> for Varu32 {
    fn parse(i: &[u8]) -> ParseResult<u32> {
        let (x, size) = varint_simd::decode(i)
            .or_else(|e| match e {
                varint_simd::VarIntDecodeError::Overflow => Err(Error::Failure(Reason::InvalidVarInt)),
                varint_simd::VarIntDecodeError::NotEnoughBytes => Err(Error::Incomplete(None))
            })?;
        
        Ok((&i[size as usize..], x))
    }
}

impl<'i> Parse<'i, u64> for Varu64 {
    fn parse(i: &[u8]) -> ParseResult<u64> {
        let (x, size) = varint_simd::decode(i)
            .or_else(|e| match e {
                varint_simd::VarIntDecodeError::Overflow => Err(Error::Failure(Reason::InvalidVarInt)),
                varint_simd::VarIntDecodeError::NotEnoughBytes => Err(Error::Incomplete(None))
            })?;
        
        Ok((&i[size as usize..], x))
    }
}

impl<'i> Parse<'i> for String {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, length) = u32::parse(i)?;

        let (i, bytes) = take(length as usize)(i)?;
        if let Ok(s) = str::from_utf8(bytes) {
            Ok((i, s.to_string()))
        } else {
            Err(Error::Failure(Reason::InvalidUTF8))
        }
    }
}

impl<'i, T: Parse<'i>> Parse<'i> for Vec<T> {
    fn parse(i: &'i [u8]) -> ParseResult<'i, Self> {
        let (mut input, length) = u32::parse(i)?;
        let mut v = Vec::with_capacity(length as usize);

        for _ in 0..length as usize {
            let (i, x) = T::parse(input)?;
            input = i;
            v.push(x);
        }
        Ok((input, v))
    }
}

impl<'i, E: Copy, T: ParseWith<'i, E>> ParseWith<'i, E> for Vec<T> {
    fn parse_with(i: &'i [u8], extra: E) -> ParseResult<'i, Self> {
        let (mut input, length) = u32::parse(i)?;
        let mut v = Vec::with_capacity(length as usize);

        for _ in 0..length as usize {
            let (i, x) = T::parse_with(input, extra)?;
            input = i;
            v.push(x);
        }
        Ok((input, v))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Header {
    magic: [u8; 4],
    version: u32,
    /// Femtoseconds per timestep.
    timescale: u32,
}

impl Parse<'_> for Header {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, magic) = u32::parse(i)?;
        let magic = magic.to_le_bytes();
        if magic != *b"svcb" {
            return Err(Error::Failure(Reason::InvalidMagic));
        }

        let (i, version) = u32::parse(i)?;
        if version != 1 {
            return Err(Error::Failure(Reason::InvalidVersion));
        }

        let (i, timescale) = u32::parse(i)?;

        Ok((i, Self { magic, version, timescale }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDeclaration {
    parent_scope_id: u32,
    scope_id: u32,
    name: String,
}

impl<'i> Parse<'i> for ScopeDeclaration {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, parent_scope_id) = u32::parse(i)?;
        let (i, scope_id) = u32::parse(i)?;
        let (i, name) = String::parse(i)?;

        Ok((i, Self { parent_scope_id, scope_id, name }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signedness {
    SignedTwosComplement,
    Unsigned,
}

impl<'i> Parse<'i> for Signedness {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, raw) = u32::parse(i)?;
        match raw {
            0 => Ok((i, Signedness::SignedTwosComplement)),
            1 => Ok((i, Signedness::Unsigned)),
            _ => Err(Error::Failure(Reason::InvalidSignedValue))
        }
    }
}

impl<'i> ParseWith<'i, usize> for BitVec {
    fn parse_with(i: &[u8], bits: usize) -> ParseResult<Self> {
        let (i, data) = take(Bit::bits_to_bytes(bits))(i)?;
        Ok((i, BitVec::new(bits, data)))
    }
}

impl<'i> ParseWith<'i, usize> for BitSlice<'i> {
    fn parse_with(i: &'i [u8], bits: usize) -> ParseResult<'i, Self> {
        let (i, data) = take(Bit::bits_to_bytes(bits))(i)?;
        Ok((i, BitSlice::new(bits, data)))
    }
}

impl<'i> ParseWith<'i, usize> for QitSlice<'i> {
    fn parse_with(i: &'i [u8], bits: usize) -> ParseResult<'i, Self> {
        let (i, data) = take(Qit::bits_to_bytes(bits))(i)?;
        Ok((i, QitSlice::new(bits, data)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumField {
    name: String,
    value: BitVec,
}

impl<'i> ParseWith<'i, usize> for EnumField {
    fn parse_with(i: &[u8], bits: usize) -> ParseResult<Self> {
        let (i, name) = String::parse(i)?;
        let (i, value) = BitVec::parse_with(i, bits)?;
        Ok((i, EnumField { name, value }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableInterpretation {
    Integer {
        storage_ids: Vec<u32>,
        msb: u32,
        lsb: u32,
        signedness: Signedness,
    },
    Enum {
        storage_id: u32,
        fields: Vec<EnumField>,
    },
    Other {
        storage_id: u32,
    }
}

impl<'i, E: StorageLookup> ParseWith<'i, &E> for VariableInterpretation {
    fn parse_with(i: &'i [u8], storages: &E) -> ParseResult<'i, Self> {
        let (i, interpretation) = u32::parse(i)?;

        match interpretation {
            0 | 2 | 3 => {
                let (i, storage_id) = u32::parse(i)?;
                if interpretation == 2 {
                    let bits = storages
                        .lookup(storage_id)
                        .ok_or_else(|| Error::Failure(Reason::InvalidStorageId))?
                        .length;

                    let (i, fields) = Vec::<EnumField>::parse_with(i, bits as usize)?;

                    Ok((i, VariableInterpretation::Enum { storage_id, fields }))
                } else {
                    Ok((i, VariableInterpretation::Other { storage_id }))
                }
            }
            1 => {
                let (i, storage_ids) = Vec::<u32>::parse(i)?;
                let (i, msb) = u32::parse(i)?;
                let (i, lsb) = u32::parse(i)?;
                let (i, signedness) = Signedness::parse(i)?;

                Ok((i, VariableInterpretation::Integer {
                    storage_ids,
                    msb,
                    lsb,
                    signedness,
                }))
            }
            _ => Err(Error::Failure(Reason::InvalidInterpretationValue))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDeclaration {
    scope_id: u32,
    name: String,
    interpretation: VariableInterpretation,
}

impl<'i, E: StorageLookup> ParseWith<'i, &E> for VariableDeclaration {
    fn parse_with(i: &'i [u8], storages: &E) -> ParseResult<'i, Self> {
        let (i, scope_id) = u32::parse(i)?;
        let (i, name) = String::parse(i)?;

        let (i, interpretation) = VariableInterpretation::parse_with(i, storages)?;

        Ok((i, VariableDeclaration { scope_id, name, interpretation }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageType {
    Binary {
        lsb: u32,
    },
    Quaternary {
        lsb: u32,
    },
    Utf8,
}

impl Parse<'_> for StorageType {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, ty) = u32::parse(i)?;
        match ty {
            0 => {
                let (i, lsb) = u32::parse(i)?;
                Ok((i,
                    StorageType::Binary {
                        lsb,
                    }
                ))
            },
            1 => {
                let (i, lsb) = u32::parse(i)?;
                Ok((i,
                    StorageType::Quaternary {
                        lsb,
                    }
                ))
            },
            2 => {
                Ok((i, StorageType::Utf8))
            },
            _ => {
                Err(Error::Failure(Reason::InvalidStorageType))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageDeclaration {
    pub id: u32,
    pub ty: StorageType,
    pub length: u32,
}

impl Parse<'_> for StorageDeclaration {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, id) = u32::parse(i)?;
        let (i, ty) = StorageType::parse(i)?;
        let (i, length) = u32::parse(i)?;

        Ok((i, Self { id, ty, length }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestep(pub u64);

impl Parse<'_> for Timestep {
    fn parse(i: &[u8]) -> ParseResult<Self> {
        let (i, timestep) = Varu64::parse(i)?;
        Ok((i, Self(timestep)))
    }
}

pub enum ValueChange<'a> {
    Binary(BitSlice<'a>),
    Quaternary(QitSlice<'a>),
    Utf8(&'a [u8]),
}

impl<'i, 'a, F: FnOnce(u32) -> Option<&'a StorageDeclaration>> ParseWith<'i, F> for ValueChange<'i> {
    fn parse_with(i: &'i [u8], f: F) -> ParseResult<'i, Self> {
        let (i, storage_id) = Varu32::parse(i)?;

        let storage = f(storage_id)
            .ok_or_else(|| Error::Failure(Reason::InvalidStorageId))?;
        
        match storage.ty {
            StorageType::Binary { lsb } => {
                let (i, bitslice) = BitSlice::parse_with(i, (storage.length - lsb) as usize)?;

                Ok((i, ValueChange::Binary(bitslice)))
            }
            StorageType::Quaternary { lsb } => {
                let (i, qitslice) = QitSlice::parse_with(i, (storage.length - lsb) as usize)?;

                Ok((i, ValueChange::Quaternary(qitslice)))
            }
            StorageType::Utf8 => {
                let (i, slice) = take(storage.length as usize)(i)?;

                Ok((i, ValueChange::Utf8(slice)))
            }
        }
    }
}

// #[derive(Clone, Copy)]
// pub struct ValueChanges<'i, 'a, L> {
//     storages: &'a L,
//     remaining: usize,
//     data: &'i [u8],
// }

// impl<'i, 'a, L> ValueChanges<'i, 'a, L> {
//     pub fn input(&self) -> &'i [u8] {
//         self.data
//     }
// }

// impl<'i, 'a, L> ParseWith<'i, &'a L> for ValueChanges<'i, 'a, L> {
//     fn parse_with(i: &'i [u8], storages: &'a L) -> ParseResult<'i, Self> {
//         let (i, count) = Varu32::parse(i)?;

//         Ok((i, Self { storages, remaining: count as usize, data: i }))
//     }
// }

// impl<'i, 'a, L: StorageLookup> Iterator for ValueChanges<'i, 'a, L> {
//     type Item = Result<ValueChange<'i>, Error>;

//     fn next(&mut self) -> Option<Self::Item> {
//         if self.remaining == 0 {
//             return None;
//         }
//         self.remaining -= 1;

//         let (i, value_change) = match ValueChange::parse_with(self.data, self.storages) {
//             Ok(x) => x,
//             Err(e) => return Some(Err(e)),
//         };
//         self.data = i;

//         Some(Ok(value_change))
//     }
// }

struct ValueChangeData<T> {
    storage_id: u32,
    offset_to_prev: u64,
    offset_to_prev_timestamp: u64,
    storage: T,
}

enum ValueChangeProxy {}

impl<T: AsRef<[u8]> + SizeInBytes> WriteData<ValueChangeProxy> for ValueChangeData<T> {
    #[inline]
    fn max_size(length: usize) -> usize {
        <u32 as WriteData>::max_size(())
        + <u64 as WriteData>::max_size(()) * 2
        + T::size_in_bytes(length)
    }

    fn write_bytes(self, length: usize, mut b: &mut [u8]) -> usize {
        let mut header = self.storage_id.write_bytes((), &mut b);
        header += self.offset_to_prev.write_bytes((), &mut b[header..]);
        header += self.offset_to_prev_timestamp.write_bytes((), &mut b[header..]);

        let bytes = T::size_in_bytes(length);

        b[header..header + bytes].copy_from_slice(self.storage.as_ref());

        header + bytes
    }
}
// impl<'a> ReadData<'a, ValueChangeProxy> for ValueChange<QitSlice<'a>> {
//     fn read_data(length: usize, b: &'a [u8]) -> (Self, usize) {
//         let (var_id, mut offset) = VarId::read_data((), b);
//         let (offset_to_prev, size) = u64::read_data((), &b[offset..]);
//         offset += size;
//         let (offset_to_prev_timestamp, size) = u64::read_data((), &b[offset..]);
//         offset += size;

//         let bytes = Qit::bits_to_bytes(qits);

//         let data = ValueChange {
//             var_id,
//             offset_to_prev,
//             offset_to_prev_timestamp,
//             qits: QitSlice::new(qits, &b[offset..offset + bytes]),
//         };

//         (data, offset + bytes)
//     }
// }

impl VariableLength for ValueChangeProxy {
    type Meta = usize;
    type DefaultReadData = ();
}

#[derive(Clone)]
struct StorageMeta {
    storage_id: u32,
    last_value_change_offset: u64,
    number_of_value_changes: u64,
    last_timestamp_offset: u64,
    last_timestamp: u64,
}

/// Used to efficiently convert from an svcb that's larger than memory
/// to a structure that can be easily traversed in order to create a
/// db that can be easily and quickly queried.
pub struct SvcbConverter {
    // All storages.
    storages: HashMap<u32, (StorageMeta, StorageDeclaration), ahash::RandomState>,

    /// A list of timestamps, stored as the delta since the previous timestamp.
    timestamp_chain: VarMmapVec<u64>,

    value_changes: VarMmapVec<ValueChangeProxy>,

    // var_tree: VarTree,
}


impl SvcbConverter {
    pub fn load_svcb(input: &[u8]) -> Result<Self, Error> {
        use crate::svcb::*;

        impl StorageLookup for HashMap<u32, StorageDeclaration, ahash::RandomState> {
            fn lookup(&self, storage_id: u32) -> Option<&StorageDeclaration> {
                self.get(&storage_id)
            }
        }

        let (mut input, _header) = Header::parse(input)?;

        let mut storages: HashMap<u32, (StorageMeta, StorageDeclaration), ahash::RandomState> = HashMap::default();
        let mut timestamp_chain = unsafe { VarMmapVec::create()? };
        let mut value_changes = unsafe { VarMmapVec::create()? };
        let mut storage_declarations: HashMap<_, _, ahash::RandomState> = HashMap::default();

        let mut processed_commands_count = 0;
        let start = Instant::now();

        let mut timestamp_acc = 0;
        let mut timestep_offset = 0;
        let mut number_of_timestamps = 0;

        while input.len() > 0 {
            let (i, block_type) = u8::parse(input)?;

            input = match block_type {
                // Scope Declaration.
                0 => {
                    let (i, scope) = ScopeDeclaration::parse(i)?;
                    println!("received scope declaration: {:#?}", scope);

                    processed_commands_count += 1;
                    i
                }
                // Variable Declaration.
                1 => {
                    let (i, variable) = VariableDeclaration::parse_with(i, &storage_declarations)?;
                    println!("received variable declaration: {:#?}", variable);

                    processed_commands_count += 1;
                    i
                }
                // Storage Declaration.
                2 => {
                    let (i, storage) = StorageDeclaration::parse(i)?;
                    storage_declarations.insert(storage.id, storage)
                        .ok_or(Error::Failure(Reason::DuplicatedStorageId))?;
                    
                    processed_commands_count += 1;
                    i
                }
                // Value Change
                3 => {
                    // let (_, mut value_changes) = ValueChanges::parse_with(i, &storage_declarations)?;
                    let (mut i, count) = Varu32::parse(i)?;

                    for _ in 0..count {
                        let mut storage_meta = None;
                        let (i2, value_change) = ValueChange::parse_with(i, |storage_id| {
                            // storages.get(&storage_id)
                            storages.get_mut(&storage_id)
                                .map(|(storage, declaration)| {
                                    storage_meta = Some((storage, declaration as &StorageDeclaration));
                                    declaration as _
                                })
                        })?;
                        i = i2;
                        let (storage, declaration) = storage_meta.unwrap();

                        storage.last_value_change_offset = match value_change {
                            ValueChange::Binary(bits) => {
                                value_changes.push(declaration.length as _, ValueChangeData {
                                    storage_id: storage.storage_id,
                                    offset_to_prev: value_changes.current_offset() - storage.last_value_change_offset,
                                    offset_to_prev_timestamp: timestep_offset - storage.last_timestamp_offset,
                                    storage: bits,
                                })
                            },
                            ValueChange::Quaternary(qits) => {
                                value_changes.push(declaration.length as _, ValueChangeData {
                                    storage_id: storage.storage_id,
                                    offset_to_prev: value_changes.current_offset() - storage.last_value_change_offset,
                                    offset_to_prev_timestamp: timestep_offset - storage.last_timestamp_offset,
                                    storage: qits,
                                })
                            }
                            ValueChange::Utf8(bytes) => {
                                value_changes.push(declaration.length as _, ValueChangeData {
                                    storage_id: storage.storage_id,
                                    offset_to_prev: value_changes.current_offset() - storage.last_value_change_offset,
                                    offset_to_prev_timestamp: timestep_offset - storage.last_timestamp_offset,
                                    storage: bytes,
                                })
                            }
                        };
                        storage.number_of_value_changes += 1;
                        storage.last_timestamp_offset = timestep_offset;
                        storage.last_timestamp = timestamp_acc;
                    }

                    processed_commands_count += 1;
                    i
                }
                // Timestep
                4 => {
                    let (i, Timestep(timestep)) = Timestep::parse(i)?;

                    timestep_offset = timestamp_chain.push((), timestep);
                    timestamp_acc += timestep;

                    processed_commands_count += 1;
                    number_of_timestamps += 1;

                    println!("received timestep: {}", timestep);
                    
                    i
                }
                _ => {
                    return Err(Error::Failure(Reason::InvalidBlockType));
                }
            }
        }        
        
        Ok(Self {
            storages,
            timestamp_chain,
            value_changes,
            // var_tree: todo!(),
        })
    }
}