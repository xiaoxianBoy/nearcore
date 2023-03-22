use std::io;

use near_o11y::pretty;

use crate::DBCol;

pub(crate) mod rocksdb;

mod colddb;
mod splitdb;

pub mod refcount;
mod slice;
mod testdb;

mod database_tests;

pub use self::colddb::ColdDB;
pub use self::rocksdb::RocksDB;
pub use self::splitdb::SplitDB;

pub use self::slice::DBSlice;
pub use self::testdb::TestDB;

pub const HEAD_KEY: &[u8; 4] = b"HEAD";
pub const TAIL_KEY: &[u8; 4] = b"TAIL";
pub const CHUNK_TAIL_KEY: &[u8; 10] = b"CHUNK_TAIL";
pub const FORK_TAIL_KEY: &[u8; 9] = b"FORK_TAIL";
pub const HEADER_HEAD_KEY: &[u8; 11] = b"HEADER_HEAD";
pub const FINAL_HEAD_KEY: &[u8; 10] = b"FINAL_HEAD";
pub const LATEST_KNOWN_KEY: &[u8; 12] = b"LATEST_KNOWN";
pub const LARGEST_TARGET_HEIGHT_KEY: &[u8; 21] = b"LARGEST_TARGET_HEIGHT";
pub const GENESIS_JSON_HASH_KEY: &[u8; 17] = b"GENESIS_JSON_HASH";
pub const GENESIS_STATE_ROOTS_KEY: &[u8; 19] = b"GENESIS_STATE_ROOTS";
pub const COLD_HEAD_KEY: &[u8; 9] = b"COLD_HEAD";

#[derive(Default, Debug)]
pub struct DBTransaction {
    pub(crate) ops: Vec<DBOp>,
}

pub(crate) enum DBOp {
    /// Sets `key` to `value`, without doing any checks.
    Set { col: DBCol, key: Vec<u8>, value: Vec<u8> },
    /// Sets `key` to `value`, and additionally debug-checks that the value is
    /// not overwritten.
    Insert { col: DBCol, key: Vec<u8>, value: Vec<u8> },
    /// Modifies a reference-counted column. `value` includes both the value per
    /// se and a refcount at the end.
    UpdateRefcount { col: DBCol, key: Vec<u8>, value: Vec<u8> },
    /// Deletes sepecific `key`.
    Delete { col: DBCol, key: Vec<u8> },
    /// Deletes all data from a column.
    DeleteAll { col: DBCol },
    /// Deletes [`from`, `to`) key range, i.e. including `from` and excluding `to`
    DeleteRange { col: DBCol, from: Vec<u8>, to: Vec<u8> },
}

impl DBOp {
    pub fn col(&self) -> DBCol {
        *match self {
            DBOp::Set { col, .. } => col,
            DBOp::Insert { col, .. } => col,
            DBOp::UpdateRefcount { col, .. } => col,
            DBOp::Delete { col, .. } => col,
            DBOp::DeleteAll { col } => col,
            DBOp::DeleteRange { col, .. } => col,
        }
    }
}

impl std::fmt::Debug for DBOp {
    // Mostly default implementation generated by vs code but with pretty keys and values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Set { col, key, value } => f
                .debug_struct("Set")
                .field("col", col)
                .field("key", &pretty::StorageKey(key))
                .field("value", &pretty::AbbrBytes(value))
                .finish(),
            Self::Insert { col, key, value } => f
                .debug_struct("Insert")
                .field("col", col)
                .field("key", &pretty::StorageKey(key))
                .field("value", &pretty::AbbrBytes(value))
                .finish(),
            Self::UpdateRefcount { col, key, value } => f
                .debug_struct("UpdateRefcount")
                .field("col", col)
                .field("key", &pretty::StorageKey(key))
                .field("value", &pretty::AbbrBytes(value))
                .finish(),
            Self::Delete { col, key } => f
                .debug_struct("Delete")
                .field("col", col)
                .field("key", &pretty::StorageKey(key))
                .finish(),
            Self::DeleteAll { col } => f.debug_struct("DeleteAll").field("col", col).finish(),
            Self::DeleteRange { col, from, to } => f
                .debug_struct("DeleteRange")
                .field("col", col)
                .field("from", from)
                .field("to", to)
                .finish(),
        }
    }
}

impl DBTransaction {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    pub fn set(&mut self, col: DBCol, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push(DBOp::Set { col, key, value });
    }

    pub fn insert(&mut self, col: DBCol, key: Vec<u8>, value: Vec<u8>) {
        assert!(col.is_insert_only(), "can't insert: {col:?}");
        self.ops.push(DBOp::Insert { col, key, value });
    }

    pub fn update_refcount(&mut self, col: DBCol, key: Vec<u8>, value: Vec<u8>) {
        assert!(col.is_rc(), "can't update refcount: {col:?}");
        self.ops.push(DBOp::UpdateRefcount { col, key, value });
    }

    pub fn delete(&mut self, col: DBCol, key: Vec<u8>) {
        self.ops.push(DBOp::Delete { col, key });
    }

    pub fn delete_all(&mut self, col: DBCol) {
        self.ops.push(DBOp::DeleteAll { col });
    }

    pub fn delete_range(&mut self, col: DBCol, from: Vec<u8>, to: Vec<u8>) {
        self.ops.push(DBOp::DeleteRange { col, from, to });
    }

    pub fn merge(&mut self, other: DBTransaction) {
        self.ops.extend(other.ops)
    }
}

pub type DBIteratorItem = io::Result<(Box<[u8]>, Box<[u8]>)>;
pub type DBIterator<'a> = Box<dyn Iterator<Item = DBIteratorItem> + 'a>;

pub trait Database: Sync + Send {
    /// Returns raw bytes for given `key` ignoring any reference count decoding
    /// if any.
    ///
    /// Note that when reading reference-counted column, the reference count
    /// will not be decoded or stripped from the value.  Similarly, cells with
    /// non-positive reference count will be returned as existing.
    ///
    /// You most likely will want to use [`Self::get_with_rc_stripped`] to
    /// properly handle reference-counted columns.
    fn get_raw_bytes(&self, col: DBCol, key: &[u8]) -> io::Result<Option<DBSlice<'_>>>;

    /// Returns value for given `key` forcing a reference count decoding.
    ///
    /// **Panics** if the column is not reference counted.
    fn get_with_rc_stripped(&self, col: DBCol, key: &[u8]) -> io::Result<Option<DBSlice<'_>>> {
        assert!(col.is_rc());
        Ok(self.get_raw_bytes(col, key)?.and_then(DBSlice::strip_refcount))
    }

    /// Iterate over all items in given column in lexicographical order sorted
    /// by the key.
    ///
    /// When reading reference-counted column, the reference count will be
    /// correctly stripped.  Furthermore, elements with non-positive reference
    /// count will be treated as non-existing (i.e. they’re going to be
    /// skipped).  For all other columns, the value is returned directly from
    /// the database.
    fn iter<'a>(&'a self, col: DBCol) -> DBIterator<'a>;

    /// Iterate over items in given column whose keys start with given prefix.
    ///
    /// This is morally equivalent to [`Self::iter`] with a filter discarding
    /// keys which do not start with given `key_prefix` (but faster).  The items
    /// are returned in lexicographical order sorted by the key.
    fn iter_prefix<'a>(&'a self, col: DBCol, key_prefix: &'a [u8]) -> DBIterator<'a>;

    /// Iterate over items in given column whose keys are between [lower_bound, upper_bound)
    ///
    /// Upper_bound key is not included.
    /// If lower_bound is None - the iterator starts from the first key.
    /// If upper_bound is None - iterator continues to the last key.
    fn iter_range<'a>(
        &'a self,
        col: DBCol,
        lower_bound: Option<&'a [u8]>,
        upper_bound: Option<&'a [u8]>,
    ) -> DBIterator<'a>;

    /// Iterate over items in given column bypassing reference count decoding if
    /// any.
    ///
    /// This is like [`Self::iter`] but it returns raw bytes as stored in the
    /// database.  For reference-counted columns this means that the reference
    /// count will not be decoded or stripped from returned value and elements
    /// with non-positive reference count will be included in the iterator.
    ///
    /// If in doubt, use [`Self::iter`] instead.  Unless you’re doing something
    /// low-level with the database (e.g. doing a migration), you probably don’t
    /// want this method.
    fn iter_raw_bytes<'a>(&'a self, col: DBCol) -> DBIterator<'a>;

    /// Atomically apply all operations in given batch at once.
    fn write(&self, batch: DBTransaction) -> io::Result<()>;

    /// Flush all in-memory data to disk.
    ///
    /// This is a no-op for in-memory databases.
    fn flush(&self) -> io::Result<()>;

    /// Compact database representation.
    ///
    /// If the database supports it a form of compaction, calling this function
    /// is blocking until compaction finishes. Otherwise, this is a no-op.
    fn compact(&self) -> io::Result<()>;

    /// Returns statistics about the database if available.
    fn get_store_statistics(&self) -> Option<StoreStatistics>;
}

fn assert_no_overwrite(col: DBCol, key: &[u8], value: &[u8], old_value: &[u8]) {
    assert!(
        value == old_value,
        "\
write once column overwritten
col: {col}
key: {key:?}
"
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatsValue {
    Count(i64),
    Sum(i64),
    Percentile(u32, f64),
    ColumnValue(DBCol, i64),
}

#[derive(Debug, PartialEq)]
pub struct StoreStatistics {
    pub data: Vec<(String, Vec<StatsValue>)>,
}
