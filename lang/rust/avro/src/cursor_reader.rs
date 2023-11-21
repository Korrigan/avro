// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Logic handling reading from Avro format at user level.
use crate::{
    decode::decode,
    reader::{from_avro_datum, read_codec},
    schema::Schema,
    types::Value,
    util, AvroResult, Codec, Error,
};
use serde_json::from_slice;
use std::{
    collections::HashMap,
    io::{Cursor, Read},
    ops::Range,
};

// Header stuff to be shared between blocks
#[derive(Debug, Clone)]
pub struct Header {
    pub marker: [u8; 16],
    pub codec: Codec,
    pub writer_schema: Schema,
    pub user_metadata: HashMap<String, Vec<u8>>,
}

impl Header {
    pub fn new<R: Read>(reader: &mut R) -> AvroResult<Header> {
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).map_err(Error::ReadHeader)?;

        if buf != [b'O', b'b', b'j', 1u8] {
            return Err(Error::HeaderMagic);
        }

        let meta_schema: Schema = Schema::Map(Box::new(Schema::Bytes));
        let Value::Map(metadata) = decode(&meta_schema, reader)? else {
            return Err(Error::GetHeaderMetadata);
        };

        let writer_schema = read_writer_schema(&metadata)?;
        let codec = read_codec(&metadata)?;
        let mut user_metadata = HashMap::new();

        for (key, value) in metadata {
            if key == "avro.schema" || key == "avro.codec" {
                // already processed
            } else if key.starts_with("avro.") {
                warn!("Ignoring unknown metadata key: {}", key);
            } else {
                read_user_metadata(&mut user_metadata, key, value);
            }
        }

        let mut marker = [0u8; 16];
        reader.read_exact(&mut marker).map_err(Error::ReadMarker)?;

        Ok(Header {
            marker,
            codec,
            writer_schema,
            user_metadata,
        })
    }
}

pub fn read_writer_schema(metadata: &HashMap<String, Value>) -> AvroResult<Schema> {
    let json: serde_json::Value = metadata
        .get("avro.schema")
        .and_then(|bytes| {
            if let Value::Bytes(ref bytes) = *bytes {
                from_slice(bytes.as_ref()).ok()
            } else {
                None
            }
        })
        .ok_or(Error::GetAvroSchemaFromMap)?;
    Ok(Schema::parse(&json)?)
    // if !self.schemata.is_empty() {
    //     let rs = ResolvedSchema::try_from(self.schemata.clone())?;
    //     let names: Names = rs
    //         .get_names()
    //         .iter()
    //         .map(|(name, schema)| (name.clone(), (*schema).clone()))
    //         .collect();
    //     self.writer_schema = Schema::parse_with_names(&json, names)?;
    // } else {
    //     self.writer_schema = Schema::parse(&json)?;
    // }
    // Ok(())
}

pub fn read_user_metadata(user_metadata: &mut HashMap<String, Vec<u8>>, key: String, value: Value) {
    match value {
        Value::Bytes(ref vec) => {
            user_metadata.insert(key, vec.clone());
        }
        wrong => {
            warn!(
                "User metadata values must be Value::Bytes, found {:?}",
                wrong
            );
        }
    }
}

#[derive(Debug)]
pub struct Block {
    header: Header,
    buf: Vec<u8>,
    pub buf_size: usize,
    pub message_count: usize,
}

impl Block {
    pub fn new<R: Read>(header: &Header, reader: &mut R) -> AvroResult<Self> {
        let message_count = util::read_long(reader)? as usize;
        let buf_size = util::read_long(reader)? as usize;
        let mut buf = vec![0; buf_size];

        // Read contents
        reader.read_exact(&mut buf).map_err(Error::ReadIntoBuf)?;

        // Read end marker
        let mut marker = [0u8; 16];
        reader
            .read_exact(&mut marker)
            .map_err(Error::ReadBlockMarker)?;

        if marker != header.marker {
            println!(
                "Marker error, got {:?} expected {:?}",
                marker, header.marker
            );
            return Err(Error::GetBlockMarker);
        }

        // NOTE (JAB): This doesn't fit this Reader pattern very well.
        // `self.buf` is a growable buffer that is reused as the reader is iterated.
        // For non `Codec::Null` variants, `decompress` will allocate a new `Vec`
        // and replace `buf` with the new one, instead of reusing the same buffer.
        // We can address this by using some "limited read" type to decode directly
        // into the buffer. But this is fine, for now.
        header.codec.decompress(&mut buf)?;
        Ok(Block {
            header: header.clone(),
            buf,
            buf_size,
            message_count,
        })
    }

    pub fn message_at(&self, offset: usize) -> AvroResult<Value> {
        BlockIterator::with_buf(self, &self.buf[offset..]).next_message()
    }

    pub fn iter_messages_between(&self, range: Range<usize>) -> impl Iterator<Item = Value> + '_ {
        BlockIterator::with_buf(self, &self.buf[range])
    }

    pub fn iter_messages(&self) -> impl Iterator<Item = Value> + '_ {
        self.iter()
    }

    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        BlockIterator::new(self)
    }
}

pub struct BlockIntoIterator {
    header: Header,
    buf_size: u64,
    reader: Cursor<Vec<u8>>,
    errored: bool,
}

impl BlockIntoIterator {
    pub fn new(block: Block) -> Self {
        Self {
            header: block.header,
            buf_size: block.buf.len() as u64,
            reader: Cursor::new(block.buf),
            errored: false,
        }
    }
}

impl Iterator for BlockIntoIterator {
    type Item = Value;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored || self.reader.position() >= self.buf_size {
            self.errored = true;
            return None;
        }
        from_avro_datum(&self.header.writer_schema, &mut self.reader, None).ok()
    }
}

impl IntoIterator for Block {
    type Item = Value;
    type IntoIter = BlockIntoIterator;

    fn into_iter(self) -> Self::IntoIter {
        BlockIntoIterator::new(self)
    }
}

pub struct BlockIterator<'a> {
    header: &'a Header,
    reader: Cursor<&'a [u8]>,
    buf_size: u64,
    errored: bool,
}

impl<'a> BlockIterator<'a> {
    pub fn with_buf(block: &'a Block, buf: &'a [u8]) -> Self {
        Self {
            header: &block.header,
            reader: Cursor::new(buf),
            buf_size: buf.len() as u64,
            errored: false,
        }
    }

    pub fn new(block: &'a Block) -> Self {
        Self {
            header: &block.header,
            reader: Cursor::new(&block.buf),
            buf_size: block.buf.len() as u64,
            errored: false,
        }
    }

    fn next_message(&mut self) -> AvroResult<Value> {
        from_avro_datum(&self.header.writer_schema, &mut self.reader, None)
    }
}

impl<'a> Iterator for BlockIterator<'a> {
    type Item = Value;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored || self.reader.position() >= self.buf_size {
            return None;
        }
        match self.next_message() {
            Ok(v) => Some(v),
            Err(e) => {
                // TODO! maybe better handle error ?
                println!("Error in BlockIterator.next: {:?}", e);
                self.errored = true;
                None
            }
        }
    }
}

pub struct CursorReader<'a, T: Clone + AsRef<[u8]>> {
    buf: &'a T,
    pub header: Header,
    pub header_size: usize,
}

impl<'a, T: Clone + AsRef<[u8]>> CursorReader<'a, T> {
    pub fn new(buf: &'a T) -> AvroResult<CursorReader<'a, T>> {
        let mut header_reader = Cursor::new(buf);
        let header = Header::new(&mut header_reader)?;
        let header_size = header_reader.position() as usize;
        Ok(Self {
            buf,
            header,
            header_size,
        })
    }

    pub fn block_at(&self, offset: usize) -> AvroResult<Block> {
        Ok(Block::new(
            &self.header,
            &mut Cursor::new(&self.buf.as_ref()[offset..]),
        )?)
    }

    pub fn iter_blocks(&self) -> impl Iterator<Item = Block> + '_ {
        BlocksIter::new(&self.header, &self.buf.as_ref()[self.header_size..])
    }

    pub fn blocks_between(&self, range: Range<usize>) -> impl Iterator<Item = Block> + '_ {
        let buf = self.buf.as_ref();
        BlocksIter::new(&self.header, &buf[range])
    }
}

pub struct BlocksIter<'a> {
    header: &'a Header,
    reader: Cursor<&'a [u8]>,
    buf_size: u64,
    errored: bool,
}

impl<'a> BlocksIter<'a> {
    fn new(header: &'a Header, buf: &'a [u8]) -> Self {
        Self {
            header,
            reader: Cursor::new(buf),
            buf_size: buf.len() as u64,
            errored: false,
        }
    }
}

impl<'a> Iterator for BlocksIter<'a> {
    type Item = Block;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored || self.reader.position() >= self.buf_size {
            return None;
        }
        match Block::new(self.header, &mut self.reader) {
            Ok(v) => Some(v),
            Err(_) => {
                // TODO! maybe better handle error ?
                self.errored = true;
                None
            }
        }
    }
}
