//! GGUF header reader.
//!
//! Reads the header of a GGUF v2/v3 file — magic, version, metadata key/values and tensor
//! infos — without reading tensor data. Weight sizes are computed exactly from each tensor's
//! dimensions and its ggml type's block layout.
//!
//! Every count in the file is untrusted: lengths are checked against the bytes left in the file
//! before anything is allocated, recursion is bounded, and large arrays (tokenizer vocabularies)
//! are validated and skipped instead of stored. Malformed or truncated files return a
//! [`GgufError`]; the reader never panics on bad input.

use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// `GGUF` read as a little-endian `u32`.
const MAGIC: [u8; 4] = *b"GGUF";
/// Default tensor data alignment when `general.alignment` is absent.
const DEFAULT_ALIGNMENT: u64 = 32;
/// llama.cpp's `GGML_MAX_DIMS`.
const MAX_DIMS: u32 = 4;
/// Arrays longer than this are validated but not kept in memory (vocabularies, merges).
const MAX_STORED_ARRAY_LEN: u64 = 65_536;
/// Nested arrays deeper than this are rejected.
const MAX_ARRAY_DEPTH: u32 = 3;
/// No real string in a GGUF header comes close (chat templates are tens of KB).
const MAX_STRING_LEN: u64 = 64 * 1024 * 1024;
/// Upper bound on tensors; the largest real models have tens of thousands.
const MAX_TENSORS: u64 = 1_000_000;
/// Upper bound on metadata entries.
const MAX_KVS: u64 = 1_000_000;

/// Why a GGUF file could not be read.
#[derive(Debug)]
pub enum GgufError {
    Io(io::Error),
    /// The file does not start with `GGUF`.
    NotGguf,
    /// A big-endian GGUF file (version bytes swapped).
    BigEndian,
    /// A version canfit cannot read (v1, or newer than v3).
    UnsupportedVersion(u32),
    /// The file ends inside the header.
    Truncated {
        what: &'static str,
    },
    /// A field holds an impossible value.
    Malformed(String),
    /// A tensor uses a ggml type canfit does not know.
    UnknownTensorType {
        tensor: String,
        type_id: u32,
    },
}

impl fmt::Display for GgufError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GgufError::Io(err) => write!(f, "{err}"),
            GgufError::NotGguf => {
                write!(f, "it is not a GGUF file (it does not start with \"GGUF\")")
            }
            GgufError::BigEndian => {
                write!(f, "it is a big-endian GGUF file, which canfit can't read")
            }
            GgufError::UnsupportedVersion(v) => {
                write!(f, "it uses GGUF version {v}; canfit reads versions 2 and 3")
            }
            GgufError::Truncated { what } => write!(
                f,
                "the file is truncated (it ends inside the {what}); the download may be incomplete"
            ),
            GgufError::Malformed(detail) => write!(f, "the file is malformed: {detail}"),
            GgufError::UnknownTensorType { tensor, type_id } => write!(
                f,
                "tensor \"{tensor}\" uses ggml type {type_id}, which this version of canfit doesn't know"
            ),
        }
    }
}

impl std::error::Error for GgufError {}

/// A metadata value. Large arrays are kept as their length only.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    String(String),
    Array(Array),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Array {
    /// GGUF value type id of the elements.
    pub element_type: u32,
    pub len: u64,
    /// The elements, or `None` when the array was too long to keep.
    pub items: Option<Vec<Value>>,
}

impl Value {
    /// The value as an unsigned integer, for any non-negative integer type.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::U8(v) => Some(v.into()),
            Value::U16(v) => Some(v.into()),
            Value::U32(v) => Some(v.into()),
            Value::U64(v) => Some(v),
            Value::I8(v) => u64::try_from(v).ok(),
            Value::I16(v) => u64::try_from(v).ok(),
            Value::I32(v) => u64::try_from(v).ok(),
            Value::I64(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The stored elements of an array value.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(Array {
                items: Some(items), ..
            }) => Some(items),
            _ => None,
        }
    }
}

/// One tensor's header entry.
#[derive(Debug, Clone, PartialEq)]
pub struct TensorInfo {
    pub name: String,
    /// Dimensions, innermost first (ggml `ne`).
    pub dims: Vec<u64>,
    pub ggml_type: GgmlType,
    /// Offset of the tensor's data relative to the start of the data section.
    pub offset: u64,
    /// Number of elements.
    pub elements: u64,
    /// Exact size of the tensor's data in bytes.
    pub bytes: u64,
}

/// A parsed GGUF header.
#[derive(Debug, Clone)]
pub struct GgufFile {
    pub version: u32,
    pub metadata: Vec<(String, Value)>,
    pub tensors: Vec<TensorInfo>,
    pub file_size: u64,
}

impl GgufFile {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.metadata
            .iter()
            .find_map(|(k, v)| (k == key).then_some(v))
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(Value::as_u64)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    /// Exact size of all tensor data in this file.
    pub fn tensor_bytes(&self) -> u64 {
        self.tensors.iter().map(|t| t.bytes).sum()
    }

    pub fn tensor_elements(&self) -> u64 {
        self.tensors.iter().map(|t| t.elements).sum()
    }
}

/// Reads the header of the GGUF file at `path`.
pub fn read_file(path: &Path) -> Result<GgufFile, GgufError> {
    let file = File::open(path).map_err(GgufError::Io)?;
    let len = file.metadata().map_err(GgufError::Io)?.len();
    read(BufReader::with_capacity(1 << 16, file), len)
}

/// Reads a GGUF header from `source`, whose total length is `len` bytes.
pub fn read<R: Read>(source: R, len: u64) -> Result<GgufFile, GgufError> {
    let mut r = Reader {
        inner: source,
        pos: 0,
        len,
    };

    let mut magic = [0u8; 4];
    r.bytes(&mut magic, "file signature")?;
    if magic != MAGIC {
        return Err(GgufError::NotGguf);
    }
    let version = r.u32("version")?;
    if version > 0xFFFF && version.swap_bytes() <= 0xFFFF {
        return Err(GgufError::BigEndian);
    }
    if !(2..=3).contains(&version) {
        return Err(GgufError::UnsupportedVersion(version));
    }

    let tensor_count = r.u64("tensor count")?;
    let kv_count = r.u64("metadata count")?;
    // A tensor info takes at least 8 (name len) + 4 (n_dims) + 8 (one dim) + 4 + 8 bytes; a
    // key/value at least 8 (key len) + 4 (type) + 1 byte.
    if tensor_count > MAX_TENSORS || tensor_count.saturating_mul(32) > r.remaining() {
        return Err(GgufError::Malformed(format!(
            "it declares {tensor_count} tensors, more than the file can hold"
        )));
    }
    if kv_count > MAX_KVS || kv_count.saturating_mul(13) > r.remaining() {
        return Err(GgufError::Malformed(format!(
            "it declares {kv_count} metadata entries, more than the file can hold"
        )));
    }

    let mut metadata = Vec::with_capacity(bounded_capacity(kv_count));
    for _ in 0..kv_count {
        let key = r.string("metadata key")?;
        let value_type = r.u32("metadata value type")?;
        let value = r.value(value_type, 0)?;
        metadata.push((key, value));
    }

    let alignment = match metadata.iter().find(|(k, _)| k == "general.alignment") {
        None => DEFAULT_ALIGNMENT,
        Some((_, v)) => match v.as_u64() {
            Some(a) if a > 0 && a.is_power_of_two() && a <= 1 << 20 => a,
            _ => {
                return Err(GgufError::Malformed(
                    "general.alignment is not a power of two".to_string(),
                ));
            }
        },
    };

    let mut tensors = Vec::with_capacity(bounded_capacity(tensor_count));
    for _ in 0..tensor_count {
        tensors.push(r.tensor_info()?);
    }

    let data_offset = r.pos.div_ceil(alignment) * alignment;
    for t in &tensors {
        if t.offset % alignment != 0 {
            return Err(GgufError::Malformed(format!(
                "tensor \"{}\" is not aligned to {alignment} bytes",
                t.name
            )));
        }
        let end = data_offset
            .checked_add(t.offset)
            .and_then(|o| o.checked_add(t.bytes));
        match end {
            Some(end) if end <= len => {}
            _ => {
                return Err(GgufError::Truncated {
                    what: "tensor data",
                });
            }
        }
    }

    Ok(GgufFile {
        version,
        metadata,
        tensors,
        file_size: len,
    })
}

fn bounded_capacity(count: u64) -> usize {
    usize::try_from(count.min(4096)).unwrap_or(4096)
}

struct Reader<R> {
    inner: R,
    pos: u64,
    len: u64,
}

impl<R: Read> Reader<R> {
    fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.pos)
    }

    fn bytes(&mut self, buf: &mut [u8], what: &'static str) -> Result<(), GgufError> {
        if (buf.len() as u64) > self.remaining() {
            return Err(GgufError::Truncated { what });
        }
        self.inner.read_exact(buf).map_err(|err| {
            if err.kind() == io::ErrorKind::UnexpectedEof {
                GgufError::Truncated { what }
            } else {
                GgufError::Io(err)
            }
        })?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    fn skip(&mut self, n: u64, what: &'static str) -> Result<(), GgufError> {
        if n > self.remaining() {
            return Err(GgufError::Truncated { what });
        }
        let copied =
            io::copy(&mut (&mut self.inner).take(n), &mut io::sink()).map_err(GgufError::Io)?;
        if copied != n {
            return Err(GgufError::Truncated { what });
        }
        self.pos += n;
        Ok(())
    }

    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], GgufError> {
        let mut buf = [0u8; N];
        self.bytes(&mut buf, what)?;
        Ok(buf)
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, GgufError> {
        Ok(u32::from_le_bytes(self.array(what)?))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, GgufError> {
        Ok(u64::from_le_bytes(self.array(what)?))
    }

    fn string_len(&mut self, what: &'static str) -> Result<u64, GgufError> {
        let len = self.u64(what)?;
        if len > MAX_STRING_LEN {
            return Err(GgufError::Malformed(format!(
                "a {what} claims to be {len} bytes long"
            )));
        }
        if len > self.remaining() {
            return Err(GgufError::Truncated { what });
        }
        Ok(len)
    }

    fn string(&mut self, what: &'static str) -> Result<String, GgufError> {
        let len = self.string_len(what)?;
        // `len` is bounded by the bytes left in the file and by MAX_STRING_LEN.
        let mut buf = vec![0u8; usize::try_from(len).unwrap_or(0)];
        self.bytes(&mut buf, what)?;
        // Some converters wrote non-UTF-8 bytes into strings; keep what we can.
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn value(&mut self, value_type: u32, depth: u32) -> Result<Value, GgufError> {
        const W: &str = "metadata value";
        Ok(match value_type {
            0 => Value::U8(self.array::<1>(W)?[0]),
            1 => Value::I8(i8::from_le_bytes(self.array(W)?)),
            2 => Value::U16(u16::from_le_bytes(self.array(W)?)),
            3 => Value::I16(i16::from_le_bytes(self.array(W)?)),
            4 => Value::U32(u32::from_le_bytes(self.array(W)?)),
            5 => Value::I32(i32::from_le_bytes(self.array(W)?)),
            6 => Value::F32(f32::from_le_bytes(self.array(W)?)),
            7 => match self.array::<1>(W)?[0] {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                b => return Err(GgufError::Malformed(format!("invalid boolean byte {b}"))),
            },
            8 => Value::String(self.string("metadata string")?),
            9 => Value::Array(self.array_value(depth)?),
            10 => Value::U64(u64::from_le_bytes(self.array(W)?)),
            11 => Value::I64(i64::from_le_bytes(self.array(W)?)),
            12 => Value::F64(f64::from_le_bytes(self.array(W)?)),
            t => {
                return Err(GgufError::Malformed(format!(
                    "unknown metadata value type {t}"
                )));
            }
        })
    }

    fn array_value(&mut self, depth: u32) -> Result<Array, GgufError> {
        if depth >= MAX_ARRAY_DEPTH {
            return Err(GgufError::Malformed(
                "arrays are nested too deeply".to_string(),
            ));
        }
        let element_type = self.u32("array type")?;
        let len = self.u64("array length")?;
        let fixed = match element_type {
            0 | 1 | 7 => Some(1),
            2 | 3 => Some(2),
            4..=6 => Some(4),
            10..=12 => Some(8),
            8 | 9 => None,
            t => {
                return Err(GgufError::Malformed(format!(
                    "unknown array element type {t}"
                )));
            }
        };
        // Every element takes at least one byte (strings and arrays at least 8).
        let min_size = fixed.unwrap_or(8);
        if len.saturating_mul(min_size) > self.remaining() {
            return Err(GgufError::Malformed(format!(
                "an array claims {len} elements, more than the file can hold"
            )));
        }
        if len <= MAX_STORED_ARRAY_LEN {
            let mut items = Vec::with_capacity(bounded_capacity(len));
            for _ in 0..len {
                items.push(self.value(element_type, depth + 1)?);
            }
            return Ok(Array {
                element_type,
                len,
                items: Some(items),
            });
        }
        match fixed {
            Some(size) => self.skip(len * size, "array")?,
            None if element_type == 8 => {
                for _ in 0..len {
                    let n = self.string_len("array string")?;
                    self.skip(n, "array string")?;
                }
            }
            None => {
                for _ in 0..len {
                    self.array_value(depth + 1)?;
                }
            }
        }
        Ok(Array {
            element_type,
            len,
            items: None,
        })
    }

    fn tensor_info(&mut self) -> Result<TensorInfo, GgufError> {
        let name = self.string("tensor name")?;
        let n_dims = self.u32("tensor info")?;
        if n_dims == 0 || n_dims > MAX_DIMS {
            return Err(GgufError::Malformed(format!(
                "tensor \"{name}\" has {n_dims} dimensions"
            )));
        }
        let mut dims = Vec::with_capacity(n_dims as usize);
        for _ in 0..n_dims {
            dims.push(self.u64("tensor info")?);
        }
        let type_id = self.u32("tensor info")?;
        let offset = self.u64("tensor info")?;
        let ggml_type = GgmlType::from_id(type_id).ok_or_else(|| GgufError::UnknownTensorType {
            tensor: name.clone(),
            type_id,
        })?;
        let elements = dims
            .iter()
            .try_fold(1u64, |acc, &d| acc.checked_mul(d))
            .ok_or_else(|| {
                GgufError::Malformed(format!("tensor \"{name}\" is impossibly large"))
            })?;
        let (block, size) = ggml_type.layout();
        if dims[0] % block != 0 {
            return Err(GgufError::Malformed(format!(
                "tensor \"{name}\" row length {} is not a multiple of the {} block size {block}",
                dims[0],
                ggml_type.name()
            )));
        }
        let bytes = (elements / block).checked_mul(size).ok_or_else(|| {
            GgufError::Malformed(format!("tensor \"{name}\" is impossibly large"))
        })?;
        Ok(TensorInfo {
            name,
            dims,
            ggml_type,
            offset,
            elements,
            bytes,
        })
    }
}

/// ggml tensor types with their storage layout (`ggml_type_traits` in ggml.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GgmlType(u32);

/// `(id, name, elements per block, bytes per block)`. Ids 4, 5, 31–33 and 36–38 were removed from
/// ggml (`Q4_2`, `Q4_3`, the `*_4_4`/`*_4_8`/`*_8_8` repacked types); files using them are
/// rejected as unknown.
const GGML_TYPES: &[(u32, &str, u64, u64)] = &[
    (0, "F32", 1, 4),
    (1, "F16", 1, 2),
    (2, "Q4_0", 32, 18),
    (3, "Q4_1", 32, 20),
    (6, "Q5_0", 32, 22),
    (7, "Q5_1", 32, 24),
    (8, "Q8_0", 32, 34),
    (9, "Q8_1", 32, 36),
    (10, "Q2_K", 256, 84),
    (11, "Q3_K", 256, 110),
    (12, "Q4_K", 256, 144),
    (13, "Q5_K", 256, 176),
    (14, "Q6_K", 256, 210),
    (15, "Q8_K", 256, 292),
    (16, "IQ2_XXS", 256, 66),
    (17, "IQ2_XS", 256, 74),
    (18, "IQ3_XXS", 256, 98),
    (19, "IQ1_S", 256, 50),
    (20, "IQ4_NL", 32, 18),
    (21, "IQ3_S", 256, 110),
    (22, "IQ2_S", 256, 82),
    (23, "IQ4_XS", 256, 136),
    (24, "I8", 1, 1),
    (25, "I16", 1, 2),
    (26, "I32", 1, 4),
    (27, "I64", 1, 8),
    (28, "F64", 1, 8),
    (29, "IQ1_M", 256, 56),
    (30, "BF16", 1, 2),
    (34, "TQ1_0", 256, 54),
    (35, "TQ2_0", 256, 66),
    (39, "MXFP4", 32, 17),
];

impl GgmlType {
    pub const F16: GgmlType = GgmlType(1);
    pub const Q8_0: GgmlType = GgmlType(8);
    pub const Q4_0: GgmlType = GgmlType(2);

    pub fn from_id(id: u32) -> Option<GgmlType> {
        GGML_TYPES
            .iter()
            .any(|&(i, ..)| i == id)
            .then_some(GgmlType(id))
    }

    fn entry(self) -> (u32, &'static str, u64, u64) {
        *GGML_TYPES
            .iter()
            .find(|&&(i, ..)| i == self.0)
            .expect("GgmlType is only built from known ids")
    }

    pub fn name(self) -> &'static str {
        self.entry().1
    }

    /// `(elements per block, bytes per block)`.
    pub fn layout(self) -> (u64, u64) {
        let (_, _, block, size) = self.entry();
        (block, size)
    }

    /// Average bytes per element.
    pub fn bytes_per_element(self) -> f64 {
        let (block, size) = self.layout();
        size as f64 / block as f64
    }
}

#[cfg(test)]
#[path = "../../tests/support/gguf_builder.rs"]
pub(crate) mod builder;

#[cfg(test)]
mod tests {
    use super::builder::{Gguf, Kv};
    use super::*;

    fn tiny() -> Vec<u8> {
        Gguf::new()
            .kv("general.architecture", Kv::Str("llama"))
            .kv("general.name", Kv::Str("tiny"))
            .kv("llama.block_count", Kv::U32(2))
            .kv("llama.attention.head_count_kv", Kv::ArrU32(vec![4, 4]))
            .kv("general.flag", Kv::Bool(true))
            .kv("general.f", Kv::F64(1.5))
            .tensor("token_embd.weight", &[64, 10], 1)
            .tensor("blk.0.attn_q.weight", &[256, 4], 12)
            .tensor("output.weight", &[64, 10], 8)
            .build()
    }

    fn parse(bytes: &[u8]) -> Result<GgufFile, GgufError> {
        read(bytes, bytes.len() as u64)
    }

    #[test]
    fn reads_metadata_and_tensors() {
        let bytes = tiny();
        let file = parse(&bytes).unwrap();
        assert_eq!(file.version, 3);
        assert_eq!(file.get_str("general.architecture"), Some("llama"));
        assert_eq!(file.get_u64("llama.block_count"), Some(2));
        let kv = file.get("llama.attention.head_count_kv").unwrap();
        assert_eq!(kv.as_array().unwrap().len(), 2);
        assert_eq!(file.get("general.flag"), Some(&Value::Bool(true)));
        assert_eq!(file.tensors.len(), 3);
        // F16 640 elements = 1280 B; Q4_K 1024 elements = 4 blocks × 144 B; Q8_0 640 = 20 × 34.
        assert_eq!(file.tensors[0].bytes, 1280);
        assert_eq!(file.tensors[1].bytes, 576);
        assert_eq!(file.tensors[2].bytes, 680);
        assert_eq!(file.tensor_bytes(), 1280 + 576 + 680);
        assert_eq!(file.tensor_elements(), 640 + 1024 + 640);
        assert_eq!(file.file_size, bytes.len() as u64);
    }

    #[test]
    fn version_2_is_accepted() {
        let bytes = Gguf::new().version(2).tensor("t", &[32], 0).build();
        assert_eq!(parse(&bytes).unwrap().version, 2);
    }

    #[test]
    fn large_arrays_are_skipped_not_stored() {
        let tokens: Vec<String> = (0..70_000).map(|i| format!("t{i}")).collect();
        let bytes = Gguf::new()
            .kv("tokenizer.ggml.tokens", Kv::ArrStr(tokens))
            .kv("tokenizer.ggml.scores", Kv::ArrF32(vec![0.0; 70_000]))
            .kv("after", Kv::U32(7))
            .build();
        let file = parse(&bytes).unwrap();
        match file.get("tokenizer.ggml.tokens") {
            Some(Value::Array(a)) => {
                assert_eq!(a.len, 70_000);
                assert!(a.items.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(file.get_u64("after"), Some(7));
    }

    #[test]
    fn wrong_magic_is_rejected() {
        let mut bytes = tiny();
        bytes[0] = b'X';
        assert!(matches!(parse(&bytes), Err(GgufError::NotGguf)));
        assert!(matches!(parse(b""), Err(GgufError::Truncated { .. })));
    }

    #[test]
    fn unsupported_versions_are_rejected() {
        let bytes = Gguf::new().version(1).build();
        assert!(matches!(
            parse(&bytes),
            Err(GgufError::UnsupportedVersion(1))
        ));
        let mut bytes = Gguf::new().build();
        bytes[4..8].copy_from_slice(&3u32.to_be_bytes());
        assert!(matches!(parse(&bytes), Err(GgufError::BigEndian)));
    }

    #[test]
    fn every_truncation_is_an_error() {
        let bytes = tiny();
        for cut in 0..bytes.len() {
            let result = parse(&bytes[..cut]);
            assert!(result.is_err(), "cut at {cut} parsed");
        }
    }

    #[test]
    fn bogus_counts_are_rejected_without_allocating() {
        let base = Gguf::new()
            .kv("a", Kv::U32(1))
            .tensor("t", &[32], 0)
            .build();
        // tensor_count at 8..16, kv_count at 16..24
        for (range, value) in [(8..16, u64::MAX), (16..24, u64::MAX), (8..16, 1 << 40)] {
            let mut bytes = base.clone();
            bytes[range].copy_from_slice(&value.to_le_bytes());
            assert!(matches!(parse(&bytes), Err(GgufError::Malformed(_))));
        }
        // Key length (first u64 after the counts).
        let mut bytes = base.clone();
        bytes[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn bogus_array_and_tensor_fields_are_rejected() {
        let huge = Gguf::new().raw_kv_array("a", 4, u64::MAX).build();
        assert!(matches!(parse(&huge), Err(GgufError::Malformed(_))));
        let deep = Gguf::new().raw_kv_nested_arrays("a", 10).build();
        assert!(matches!(parse(&deep), Err(GgufError::Malformed(_))));
        let dims = Gguf::new().tensor("t", &[1, 1, 1, 1, 1], 0).build();
        assert!(matches!(parse(&dims), Err(GgufError::Malformed(_))));
        let overflow = Gguf::new().tensor_no_data("t", &[u64::MAX, 4], 0).build();
        assert!(matches!(parse(&overflow), Err(GgufError::Malformed(_))));
        let unknown = Gguf::new().tensor_no_data("t", &[32], 4).build();
        assert!(matches!(
            parse(&unknown),
            Err(GgufError::UnknownTensorType { type_id: 4, .. })
        ));
        let ragged = Gguf::new().tensor_no_data("t", &[33], 12).build();
        assert!(matches!(parse(&ragged), Err(GgufError::Malformed(_))));
        let bad_bool = Gguf::new().raw_kv("b", 7, &[2]).build();
        assert!(matches!(parse(&bad_bool), Err(GgufError::Malformed(_))));
        let bad_type = Gguf::new().raw_kv("b", 99, &[]).build();
        assert!(matches!(parse(&bad_type), Err(GgufError::Malformed(_))));
    }

    #[test]
    fn missing_tensor_data_is_truncation() {
        let bytes = Gguf::new().tensor_no_data("t", &[1024], 0).build();
        assert!(matches!(
            parse(&bytes),
            Err(GgufError::Truncated {
                what: "tensor data"
            })
        ));
    }

    #[test]
    fn random_corruption_never_panics() {
        let base = tiny();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3000 {
            let mut bytes = base.clone();
            for _ in 0..(next() % 8 + 1) {
                let i = (next() as usize) % bytes.len();
                bytes[i] = next() as u8;
            }
            let cut = bytes.len() - (next() as usize % 16).min(bytes.len());
            let _ = parse(&bytes[..cut]);
        }
    }

    #[test]
    fn type_table_matches_known_layouts() {
        assert_eq!(GgmlType::from_id(12).unwrap().name(), "Q4_K");
        assert_eq!(GgmlType::from_id(14).unwrap().layout(), (256, 210));
        assert!((GgmlType::Q8_0.bytes_per_element() - 1.0625).abs() < 1e-9);
        assert!((GgmlType::Q4_0.bytes_per_element() - 0.5625).abs() < 1e-9);
        assert!(GgmlType::from_id(4).is_none());
        assert!(GgmlType::from_id(40).is_none());
    }
}
