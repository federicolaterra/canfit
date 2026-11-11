//! Builds tiny synthetic GGUF files for tests, so no model binaries live in the repository.
//!
//! Shared by the unit tests (`src/estimate/gguf.rs` includes it with `#[path]`) and the CLI
//! integration tests. Tensor data is zero-filled.

#![allow(dead_code)]

use std::path::Path;

pub enum Kv {
    U32(u32),
    I32(i32),
    U64(u64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Str(&'static str),
    String(String),
    ArrU32(Vec<u32>),
    ArrI32(Vec<i32>),
    ArrF32(Vec<f32>),
    ArrStr(Vec<String>),
}

struct Tensor {
    name: String,
    dims: Vec<u64>,
    type_id: u32,
    with_data: bool,
}

pub struct Gguf {
    version: u32,
    kvs: Vec<u8>,
    kv_count: u64,
    tensors: Vec<Tensor>,
}

/// `(elements per block, bytes per block)` for the types the tests use.
fn layout(type_id: u32) -> (u64, u64) {
    match type_id {
        0 => (1, 4),
        1 | 30 => (1, 2),
        2 => (32, 18),
        8 => (32, 34),
        10 => (256, 84),
        11 => (256, 110),
        12 => (256, 144),
        13 => (256, 176),
        14 => (256, 210),
        other => panic!("builder has no layout for ggml type {other}"),
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl Default for Gguf {
    fn default() -> Self {
        Self::new()
    }
}

impl Gguf {
    pub fn new() -> Self {
        Gguf {
            version: 3,
            kvs: Vec::new(),
            kv_count: 0,
            tensors: Vec::new(),
        }
    }

    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    pub fn kv(mut self, key: &str, value: Kv) -> Self {
        put_str(&mut self.kvs, key);
        let out = &mut self.kvs;
        let array = |out: &mut Vec<u8>, ty: u32, len: usize| {
            out.extend_from_slice(&9u32.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&(len as u64).to_le_bytes());
        };
        match value {
            Kv::U32(v) => {
                out.extend_from_slice(&4u32.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
            }
            Kv::I32(v) => {
                out.extend_from_slice(&5u32.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
            }
            Kv::U64(v) => {
                out.extend_from_slice(&10u32.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
            }
            Kv::F32(v) => {
                out.extend_from_slice(&6u32.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
            }
            Kv::F64(v) => {
                out.extend_from_slice(&12u32.to_le_bytes());
                out.extend_from_slice(&v.to_le_bytes());
            }
            Kv::Bool(v) => {
                out.extend_from_slice(&7u32.to_le_bytes());
                out.push(u8::from(v));
            }
            Kv::Str(s) => {
                out.extend_from_slice(&8u32.to_le_bytes());
                put_str(out, s);
            }
            Kv::String(s) => {
                out.extend_from_slice(&8u32.to_le_bytes());
                put_str(out, &s);
            }
            Kv::ArrU32(v) => {
                array(out, 4, v.len());
                v.iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
            Kv::ArrI32(v) => {
                array(out, 5, v.len());
                v.iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
            Kv::ArrF32(v) => {
                array(out, 6, v.len());
                v.iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
            Kv::ArrStr(v) => {
                array(out, 8, v.len());
                v.iter().for_each(|s| put_str(out, s));
            }
        }
        self.kv_count += 1;
        self
    }

    /// A key with an arbitrary value type id followed by raw bytes.
    pub fn raw_kv(mut self, key: &str, type_id: u32, payload: &[u8]) -> Self {
        put_str(&mut self.kvs, key);
        self.kvs.extend_from_slice(&type_id.to_le_bytes());
        self.kvs.extend_from_slice(payload);
        self.kv_count += 1;
        self
    }

    /// An array header claiming `len` elements of `element_type`, with no elements.
    pub fn raw_kv_array(self, key: &str, element_type: u32, len: u64) -> Self {
        let mut payload = element_type.to_le_bytes().to_vec();
        payload.extend_from_slice(&len.to_le_bytes());
        self.raw_kv(key, 9, &payload)
    }

    /// Arrays of arrays, `depth` levels deep, each with one element; the innermost holds a u8.
    pub fn raw_kv_nested_arrays(self, key: &str, depth: u32) -> Self {
        let mut payload = Vec::new();
        for _ in 1..depth {
            payload.extend_from_slice(&9u32.to_le_bytes());
            payload.extend_from_slice(&1u64.to_le_bytes());
        }
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&1u64.to_le_bytes());
        payload.push(0);
        self.raw_kv(key, 9, &payload)
    }

    pub fn tensor(mut self, name: &str, dims: &[u64], type_id: u32) -> Self {
        self.tensors.push(Tensor {
            name: name.to_string(),
            dims: dims.to_vec(),
            type_id,
            with_data: true,
        });
        self
    }

    /// A tensor info whose data is not written (and whose size is never computed).
    pub fn tensor_no_data(mut self, name: &str, dims: &[u64], type_id: u32) -> Self {
        self.tensors.push(Tensor {
            name: name.to_string(),
            dims: dims.to_vec(),
            type_id,
            with_data: false,
        });
        self
    }

    pub fn build(&self) -> Vec<u8> {
        let (mut out, data_len) = self.header();
        out.resize(out.len() + data_len as usize, 0);
        out
    }

    /// The header (padded to the data section) and the length of the tensor data after it.
    fn header(&self) -> (Vec<u8>, u64) {
        let mut out = b"GGUF".to_vec();
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&(self.tensors.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.kv_count.to_le_bytes());
        out.extend_from_slice(&self.kvs);

        let mut offset = 0u64;
        let mut data_len = 0u64;
        for t in &self.tensors {
            put_str(&mut out, &t.name);
            out.extend_from_slice(&(t.dims.len() as u32).to_le_bytes());
            t.dims
                .iter()
                .for_each(|d| out.extend_from_slice(&d.to_le_bytes()));
            out.extend_from_slice(&t.type_id.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            if t.with_data {
                let (block, size) = layout(t.type_id);
                let bytes = t.dims.iter().product::<u64>() / block * size;
                data_len = offset + bytes;
                offset = (offset + bytes).div_ceil(32) * 32;
            }
        }
        while !out.len().is_multiple_of(32) {
            out.push(0);
        }
        (out, data_len)
    }

    /// Writes the header and extends the file to its full size without writing the data, so
    /// huge models cost no disk space on file systems with sparse files (APFS, ext4).
    pub fn write_sparse(&self, path: &Path) {
        let (header, data_len) = self.header();
        std::fs::write(path, &header).expect("write synthetic GGUF header");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("reopen synthetic GGUF");
        file.set_len(header.len() as u64 + data_len)
            .expect("extend synthetic GGUF");
    }

    pub fn write(&self, path: &Path) {
        std::fs::write(path, self.build()).expect("write synthetic GGUF");
    }

    /// A small llama-style model: `layers` blocks, embedding 256, 8 heads, 2 KV heads,
    /// context 4096, Q4_K weights.
    pub fn llama(name: &'static str, layers: u32) -> Self {
        let mut g = Gguf::new()
            .kv("general.architecture", Kv::Str("llama"))
            .kv("general.name", Kv::Str(name))
            .kv("general.file_type", Kv::U32(15))
            .kv("llama.block_count", Kv::U32(layers))
            .kv("llama.context_length", Kv::U32(4096))
            .kv("llama.embedding_length", Kv::U32(256))
            .kv("llama.attention.head_count", Kv::U32(8))
            .kv("llama.attention.head_count_kv", Kv::U32(2))
            .tensor("token_embd.weight", &[256, 64], 12);
        for i in 0..layers {
            g = g.tensor(&format!("blk.{i}.attn_q.weight"), &[256, 256], 12);
        }
        g
    }
}
