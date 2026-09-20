use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum InodeType {
    RegularFile = 0,
    Directory = 1,
    Symlink = 2,
    ExecutableFile = 3,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WalHeader {
    #[prost(string, tag = "1")]
    pub repo_url: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub base_commit_oid: ::prost::alloc::string::String,
    #[prost(string, tag = "3")]
    pub author: ::prost::alloc::string::String,
    #[prost(string, tag = "4")]
    pub commit_message: ::prost::alloc::string::String,
    #[prost(uint64, tag = "5")]
    pub started_at_unix_secs: u64,
    #[prost(uint64, tag = "6")]
    pub wal_seq: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CreateFileMutation {
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
    #[prost(enumeration = "InodeType", tag = "2")]
    pub inode_type: i32,
    #[prost(uint32, tag = "3")]
    pub mode: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct MkdirMutation {
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WriteMutation {
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub offset: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TruncateMutation {
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub new_size: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RemoveMutation {
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RenameMutation {
    #[prost(string, tag = "1")]
    pub from_path: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub to_path: ::prost::alloc::string::String,
}

#[derive(Clone, PartialEq, ::prost::Oneof)]
pub enum WalPayload {
    #[prost(message, tag = "3")]
    Header(WalHeader),
    #[prost(message, tag = "4")]
    CreateFile(CreateFileMutation),
    #[prost(message, tag = "5")]
    Mkdir(MkdirMutation),
    #[prost(message, tag = "6")]
    Write(WriteMutation),
    #[prost(message, tag = "7")]
    Truncate(TruncateMutation),
    #[prost(message, tag = "8")]
    Remove(RemoveMutation),
    #[prost(message, tag = "9")]
    Rename(RenameMutation),
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WalEntry {
    #[prost(uint64, tag = "1")]
    pub seq: u64,
    #[prost(uint64, tag = "2")]
    pub timestamp_unix_nanos: u64,
    #[prost(oneof = "WalPayload", tags = "3, 4, 5, 6, 7, 8, 9")]
    pub payload: ::core::option::Option<WalPayload>,
}

impl WalEntry {
    /// Encodes the entry with a 4-byte big-endian length prefix.
    pub fn encode_framed(&self) -> Bytes {
        let msg_len = self.encoded_len();
        let mut buf = BytesMut::with_capacity(4 + msg_len);
        buf.put_u32(msg_len as u32);
        self.encode(&mut buf).expect("encoding WalEntry");
        buf.freeze()
    }

    /// Attempts to decode all valid framed entries from a buffer.
    /// If the final frame is truncated (e.g. crash during write),
    /// decoding stops cleanly and returns the valid entries parsed so far.
    pub fn decode_all_framed(mut data: &[u8]) -> (Vec<WalEntry>, usize) {
        let mut entries = Vec::new();
        let mut bytes_consumed = 0;

        while data.len() >= 4 {
            let msg_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if data.len() < 4 + msg_len {
                // Incomplete frame at the end of log (server crashed midway through write)
                break;
            }

            let msg_bytes = &data[4..4 + msg_len];
            match WalEntry::decode(msg_bytes) {
                Ok(entry) => {
                    entries.push(entry);
                    data = &data[4 + msg_len..];
                    bytes_consumed += 4 + msg_len;
                }
                Err(_) => {
                    // Corrupted record, stop recovery here
                    break;
                }
            }
        }

        (entries, bytes_consumed)
    }
}
