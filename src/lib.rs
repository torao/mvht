use std::cmp::min;
use std::fmt::{Debug, Formatter};
use std::fs::*;
use std::io;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::{Arc, LockResult, RwLock};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use highway::{HighwayBuilder, Key};

use crate::algorithm::Generation;
use crate::checksum::{HashRead, HashWrite};
use crate::error::Detail;
use crate::error::Detail::*;

pub(crate) mod algorithm;
pub(crate) mod checksum;
pub mod error;
pub mod inspect;

#[cfg(test)]
mod test;

/// MVHT クレートで使用する標準 Result。`error::Detail` も参照。
pub type Result<T> = std::result::Result<T, error::Detail>;

/// MVHT の実体を保存する抽象化されたストレージ。read 用または read + write 用のカーソル参照を実装することで任意の
/// デバイスに直列化することができます。
pub trait Storage {
  /// このストレージに対する read または read + write 用のカーソルを作成します。
  fn open(&self, writable: bool) -> Result<Box<dyn Cursor>>;
}

/// ローカルファイルシステムのパスをストレージとして使用するための実装です。
impl Storage for dyn AsRef<Path> {
  fn open(&self, writable: bool) -> Result<Box<dyn Cursor>> {
    Ok(Box::new(OpenOptions::new().write(writable).read(true).open(self)?))
  }
}

/// メモリ上の領域をストレージとして使用するための実装です。`drop()` された時点で記録していた内容が消滅するため
/// 調査やテストでの使用を想定しています。
pub struct MemStorage {
  buffer: Arc<RwLock<Vec<u8>>>,
}

impl MemStorage {
  /// 揮発性メモリを使用するストレージを構築します。
  pub fn new() -> MemStorage {
    Self::with(Arc::new(RwLock::new(Vec::<u8>::with_capacity(4 * 1024))))
  }

  /// 指定されたアトミック参照カウント/RWロック付きの可変バッファを使用するストレージを構築します。これは調査の目的で
  /// 外部からストレージの内容を参照することを想定しています。
  pub fn with(buffer: Arc<RwLock<Vec<u8>>>) -> MemStorage {
    MemStorage { buffer }
  }
}

impl Storage for MemStorage {
  fn open(&self, writable: bool) -> Result<Box<dyn Cursor>> {
    Ok(Box::new(MemCursor { writable, position: 0, buffer: self.buffer.clone() }))
  }
}

struct MemCursor {
  writable: bool,
  position: usize,
  buffer: Arc<RwLock<Vec<u8>>>,
}

impl Cursor for MemCursor {}

impl io::Seek for MemCursor {
  fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
    self.position = match pos {
      io::SeekFrom::Start(position) => position as usize,
      io::SeekFrom::End(position) => {
        let mut buffer = lock2io(self.buffer.write())?;
        let new_position = (buffer.len() as i64 + position) as usize;
        while buffer.len() < new_position {
          buffer.push(0u8);
        }
        new_position
      }
      io::SeekFrom::Current(position) => (self.position as i64 + position) as usize,
    };
    Ok(self.position as u64)
  }
}

impl io::Read for MemCursor {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let buffer = lock2io(self.buffer.read())?;
    let length = min(buf.len(), buffer.len() - self.position);
    (&mut buf[..]).write_all(&buffer[self.position..self.position + length])?;
    self.position += length;
    Ok(length)
  }
}

impl io::Write for MemCursor {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    if !self.writable {
      return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let mut buffer = lock2io(self.buffer.write())?;
    let length = buffer.write(buf)?;
    self.position += length;
    Ok(length)
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

/// `LockResult` を `io::Result` に変換します。
#[inline]
fn lock2io<T>(result: LockResult<T>) -> io::Result<T> {
  result.map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))
}

/// ストレージからデータの入出力を行うためのカーソルです。
pub trait Cursor: io::Seek + io::Read + io::Write {}

impl Cursor for File {}

pub type Index = algorithm::Index;

pub const INDEX_SIZE: u8 = algorithm::INDEX_SIZE;

/// HashTree から取得したハッシュ値付きのノードを表します。
#[derive(PartialEq, Eq, Copy, Clone)]
pub struct NodeHash {
  pub i: Index,
  pub j: u8,
  pub hash: Hash,
}

impl Debug for NodeHash {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
    f.debug_tuple("HashNode").field(&self.i).field(&self.j).field(&self.hash.to_str()).finish()
  }
}

/// HashTree から取得した値を表します。
#[derive(PartialEq, Eq)]
pub struct Value {
  pub i: Index,
  pub value: Vec<u8>,
}

impl Debug for Value {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
    f.debug_tuple("Value").field(&self.i).field(&hex(&self.value)).finish()
  }
}

/// ハッシュツリーからハッシュ値付きで返される値
#[derive(Debug)]
pub struct DataSet {
  values: Vec<Value>,
  hashes: Vec<NodeHash>,
}

impl DataSet {
  pub fn root_hash(&self) -> Hash {
    todo!()
  }
}

// --------------------------------------------------------------------------

pub const HASH_SIZE: usize = {
  #[cfg(feature = "highwayhash64")]
    {
      8
    }
  #[cfg(any(feature = "sha224", feature = "sha512_224"))]
    {
      28
    }
  #[cfg(any(feature = "sha256", feature = "sha512_256"))]
    {
      32
    }
  #[cfg(feature = "sha512")]
    {
      64
    }
};

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub struct Hash {
  pub value: [u8; HASH_SIZE],
}

impl Hash {
  pub fn new(hash: [u8; HASH_SIZE]) -> Hash {
    Hash { value: hash }
  }
  pub fn hash(value: &[u8]) -> Hash {
    #[cfg(feature = "highwayhash64")]
      {
        use highway::HighwayHash;
        let mut builder = HighwayBuilder::default();
        builder.write_all(value).unwrap();
        Hash::new(builder.finalize64().to_le_bytes())
      }
    #[cfg(not(feature = "highwayhash64"))]
      {
        use sha2::Digest;
        #[cfg(feature = "sha224")]
        use sha2::Sha224 as Sha2;
        #[cfg(any(feature = "sha256"))]
        use sha2::Sha256 as Sha2;
        #[cfg(feature = "sha512")]
        use sha2::Sha512 as Sha2;
        #[cfg(feature = "sha512/224")]
        use sha2::Sha512Trunc224 as Sha2;
        #[cfg(feature = "sha512/256")]
        use sha2::Sha512Trunc256 as Sha2;
        let output = Sha2::digest(value);
        debug_assert_eq!(HASH_SIZE, output.len());
        let mut hash = [0u8; HASH_SIZE];
        (&mut hash[..]).write_all(&output).unwrap();
        Hash::new(hash)
      }
  }

  pub fn combine(&self, other: &Hash) -> Hash {
    let mut value = [0u8; HASH_SIZE * 2];
    value[..HASH_SIZE].copy_from_slice(&self.value);
    value[HASH_SIZE..].copy_from_slice(&other.value);
    Hash::hash(&value)
  }

  pub fn to_str(&self) -> String {
    hex(&self.value)
  }
}

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
struct Address {
  /// MVHT のリスト構造上での位置。1 から開始します。
  pub i: Index,
  /// このノードの高さ (最も遠い葉ノードまでの距離)。0 の場合、ノードが葉ノードであることを示しています。
  pub j: u8,
  /// このノードが格納されているエントリの位置です。
  pub position: u64,
}

impl Address {
  pub fn new(i: Index, j: u8, position: u64) -> Address {
    Address { i, j, position }
  }
}

#[derive(PartialEq, Eq, Copy, Clone)]
struct Node {
  pub address: Address,
  pub hash: Hash,
}

impl Node {
  pub fn new(address: Address, hash: Hash) -> Node {
    Node { address, hash }
  }
}

impl Debug for Node {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    f.write_str(&format!(
      "Node({},{}@{}){}",
      self.address.i,
      self.address.j,
      self.address.position,
      self.hash.to_str()
    ))
  }
}

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
struct INode {
  pub node: Node,
  /// 左枝のノード
  pub left: Address,
  /// 右枝のノード
  pub right: Address,
}

impl INode {
  pub fn new(node: Node, left: Address, right: Address) -> INode {
    INode { node, left, right }
  }
}

#[derive(PartialEq, Eq, Debug)]
struct ENode {
  pub node: Node,
  pub payload: Vec<u8>,
}

#[derive(Eq, PartialEq, Debug)]
enum Root<'a> {
  None,
  INode(&'a INode),
  ENode(&'a ENode),
}

#[derive(Eq, PartialEq, Debug)]
enum STRoot {
  None,
  INode(INode, Vec<Node>),
  ENode(ENode, Vec<Node>),
}

#[derive(PartialEq, Eq, Debug)]
struct Entry {
  enode: ENode,
  inodes: Vec<INode>,
}

// --------------------------------------------------------------------------

/// HighwayHash でチェックサム用のハッシュ値を生成するためのキー (256-bit 固定値)。
const CHECKSUM_HW64_KEY: [u64; 4] =
  [0xFA5015F2E22BCFC6u64, 0xCE5A4ED9A4025C80, 0x16B9731717F6315E, 0x0F34D06AE93BD8E9];

/// ペイロードの最大サイズ。トレイラーの offset 値を u32 にするためにはエントリの直列化表現を最大でも `u32::MAX`
/// とする必要があります。したがって任意帳のペイロードは 2GB までとします。
/// この定数はビットマスクとしても使用するため 1-bit の連続で構成されている必要があります。
pub const MAX_PAYLOAD_SIZE: usize = 0x7FFFFFFF;

/// この実装のバージョン
const STORAGE_MAJOR_VERSION: u8 = 0u8;
const STORAGE_MINOR_VERSION: u8 = 0u8;
pub const STORAGE_VERSION: u8 = (STORAGE_MAJOR_VERSION << 4) | (STORAGE_MINOR_VERSION & 0x0F);

fn is_version_compatible(version: u8) -> bool {
  version >> 4 == STORAGE_MAJOR_VERSION
    && (STORAGE_MAJOR_VERSION != 0 || version & 0x0F == STORAGE_MINOR_VERSION)
}

/// MVHT ファイルの先頭に記録される識別子
pub const STORAGE_IDENTIFIER: [u8; 3] = [0x01u8, 0xF3, 0x33];

pub struct MVHT<S: Storage> {
  storage: Box<S>,
  cursor: Box<dyn Cursor>,
  last: Option<Entry>,
}

impl<S: Storage> MVHT<S> {
  /// 指定されたストレージをデータベースとして使用します。
  pub fn new(storage: S) -> Result<MVHT<S>> {
    let cursor = storage.open(true)?;
    let mut db = MVHT { storage: Box::new(storage), cursor, last: None };
    db.init()?;
    Ok(db)
  }

  fn root<'a>(&self) -> Root {
    match &self.last {
      None => Root::None,
      Some(entry) => match entry.inodes.last() {
        Some(inode) => Root::INode(inode),
        None => Root::ENode(&entry.enode),
      },
    }
  }

  fn init(&mut self) -> Result<()> {
    let length = self.cursor.seek(io::SeekFrom::End(0))?;
    match length {
      0 => {
        // マジックナンバーの書き込み
        self.cursor.write_all(&STORAGE_IDENTIFIER)?;
        self.cursor.write_u8(STORAGE_VERSION)?;
      }
      1..=3 => return Err(FileIsNotContentsOfMVHTree { message: "bad magic number" }),
      _ => {
        // マジックナンバーの確認
        let mut buffer = [0u8; 4];
        self.cursor.seek(io::SeekFrom::Start(0))?;
        self.cursor.read_exact(&mut buffer)?;
        if buffer[..3] != STORAGE_IDENTIFIER[..] {
          return Err(FileIsNotContentsOfMVHTree { message: "bad magic number" });
        } else if !is_version_compatible(buffer[3]) {
          return Err(IncompatibleVersion(buffer[3] >> 4, buffer[3] & 0x0F));
        }
      }
    }

    let length = self.cursor.seek(io::SeekFrom::End(0))?;
    let tail = if length == 4 {
      None
    } else {
      // 末尾のエントリを読み込み
      back_to_safety(self.cursor.as_mut(), 4 + 8, "The first entry is corrupted.")?;
      let offset = self.cursor.read_u32::<LittleEndian>()?;
      back_to_safety(self.cursor.as_mut(), offset + 4, "The last entry is corrupted.")?;
      let entry = read_entry(&mut self.cursor, 0)?;
      if self.cursor.stream_position()? != length {
        // 壊れたストレージから読み込んだ offset が、たまたまどこかの正しいエントリ境界を指していた場合、正しく
        // 読み込めるが結果となる位置は末尾と一致しない。
        let msg = "The last entry is corrupted.".to_string();
        return Err(DamagedStorage(msg));
      }
      Some(entry)
    };

    self.last = tail;
    self.cursor.seek(io::SeekFrom::End(0))?;
    Ok(())
  }

  /// この MVHT の現在のレベルを参照します。一つのノードも含まれていない場合は0 を返します。
  pub fn level(&self) -> u8 {
    match &self.root() {
      Root::INode(inode) => inode.node.address.j,
      Root::ENode(enode) => enode.node.address.j,
      Root::None => 0,
    }
  }

  /// 指定された値をこの MVHT に追加します。
  /// 追加された要素のインデックスとその時点のハッシュツリーのルートハッシュを返します。
  pub fn append(&mut self, value: &[u8]) -> Result<(Index, Hash)> {
    if value.len() > MAX_PAYLOAD_SIZE {
      return Err(TooLargePayload { size: value.len() });
    }

    // 葉ノードの構築
    let position = self.cursor.stream_position()?;
    let i = match self.root() {
      Root::None => 1,
      Root::ENode(enode) => enode.node.address.i + 1,
      Root::INode(inode) => inode.node.address.i + 1,
    };
    let hash = Hash::hash(value);
    let enode =
      ENode { node: Node::new(Address::new(i, 0, position), hash), payload: Vec::from(value) };

    // 中間ノードの構築
    let mut cursor = self.storage.open(false)?;
    let mut inodes = Vec::<INode>::with_capacity(INDEX_SIZE as usize);
    let mut right_hash = enode.node.hash;
    let gen = Generation::new(i);
    let mut right_to_left_inodes = gen.inodes();
    right_to_left_inodes.reverse();
    for n in right_to_left_inodes.iter() {
      debug_assert_eq!(i, n.node.i);
      debug_assert_eq!(n.node.i, n.right.i);
      debug_assert!(n.node.j >= n.right.j + 1);
      debug_assert!(n.left.j >= n.right.j);
      if let Some(left) = self.get_node(&mut cursor, n.left.i, n.left.j)? {
        let right = Address::new(n.right.i, n.right.j, position);
        let hash = left.hash.combine(&right_hash);
        let node = Node::new(Address::new(n.node.i, n.node.j, position), hash);
        let inode = INode::new(node, left.address, right);
        inodes.push(inode);
        right_hash = hash;
      } else {
        // 内部の木構造とストレージ上のデータが矛盾している
        return inconsistency(format!("cannot find the node b_{{{},{}}}", n.left.i, n.left.j));
      }
    }

    // エントリを書き込んで状態を更新
    let entry = Entry { enode, inodes };
    write_entry(&mut self.cursor, &entry)?;
    self.last = Some(entry);

    Ok((i, hash))
  }

  pub fn get(&self, i: Index) -> Result<Option<Vec<u8>>> {
    let mut cursor = self.storage.open(false)?;
    if let Some(node) = self.get_node(&mut cursor, i, 0)? {
      cursor.seek(io::SeekFrom::Start(node.address.position))?;
      let entry = read_entry_without_check(&mut cursor, node.address.position, node.address.i)?;
      let Entry { enode, .. } = entry;
      let ENode { payload, .. } = enode;
      Ok(Some(payload))
    } else {
      Ok(None)
    }
  }

  pub fn get_with_hashes(&self, _i: Index) -> Result<Option<(Vec<u8>, Vec<Hash>)>> {
    todo!()
  }

  /// 指定されたノード b_{i,j} に属しているすべての値を中間ノードのハッシュ値付きで取得します。
  pub fn get_values_with_hashes(&self, i: Index, j: u8) -> Result<Option<DataSet>> {
    let mut cursor = self.storage.open(false)?;
    let (values, branches) = match self.get_root_of_ith_generation(&mut cursor, i)? {
      STRoot::INode(root, branches) => {
        let i_min = (((i >> j) - (if algorithm::is_pbst(i, j) { 1 } else { 0 })) << j) + 1;
        let i_max = i;
        let count = i_max - i_min + 1;
        if let Some((position, _)) = search_entry_position(&mut cursor, &root, i_min, false)? {
          let mut values = Vec::<Value>::with_capacity(INDEX_SIZE as usize);
          cursor.seek(io::SeekFrom::Start(position))?;
          for x in 0..count {
            let entry = read_entry_without_check(&mut cursor, position, i_min)?;
            cursor.seek(io::SeekFrom::Current(4 + 8))?; // skip trailer: offset + checksum
            let mut payload = Vec::<u8>::with_capacity(entry.enode.payload.len());
            payload.write_all(&entry.enode.payload)?;
            debug_assert_eq!(i_min + x, entry.enode.node.address.i);
            values.push(Value { i: i_min + x, value: payload })
          }
          (values, branches)
        } else {
          return Ok(None);
        }
      }
      STRoot::ENode(enode, branches) if enode.node.address.i == i && j == 0 => {
        let mut payload = Vec::<u8>::with_capacity(enode.payload.len());
        payload.write_all(&enode.payload)?;
        let value = Value { i, value: payload };
        (vec![value], branches)
      }
      _ => return Ok(None),
    };
    let hashes = branches
      .iter()
      .map(|node| NodeHash { i: node.address.i, j: node.address.j, hash: node.hash })
      .collect();
    Ok(Some(DataSet { values, hashes }))
  }

  fn get_node(&self, cursor: &mut Box<dyn Cursor>, i: Index, j: u8) -> Result<Option<Node>> {
    if let Some((position, _)) = self.get_entry_position(cursor, i, false)? {
      cursor.seek(io::SeekFrom::Start(position))?;
      if j == 0 {
        let entry = read_entry_without_check(cursor, position, i)?;
        Ok(Some(entry.enode.node))
      } else {
        let inodes = read_inodes(cursor, position)?;
        Ok(inodes.iter().find(|inode| inode.node.address.j == j).map(|inode| inode.node))
      }
    } else {
      Ok(None)
    }
  }

  /// i-世代でのハッシュツリーのルートノードを参照します。
  fn get_root_of_ith_generation(&self, cursor: &mut Box<dyn Cursor>, i: Index) -> Result<STRoot> {
    match &self.root() {
      Root::INode(root) => {
        if root.node.address.i == i {
          return Ok(STRoot::INode(**root, vec![]));
        } else if let Some((position, branches)) = self.get_entry_position(cursor, i, true)? {
          cursor.seek(io::SeekFrom::Start(position))?;
          if i == 1 {
            let enode = read_entry_without_check(cursor, position, i)?;
            return Ok(STRoot::ENode(enode.enode, branches));
          } else {
            let inodes = read_inodes(cursor, position)?;
            if let Some(inode) = inodes.last().map(|n| *n) {
              return Ok(STRoot::INode(inode, branches));
            }
          }
        }
        Ok(STRoot::None)
      }
      Root::ENode(root) if root.node.address.i == i => {
        let mut payload = Vec::<u8>::with_capacity(root.payload.len());
        payload.write_all(&root.payload)?;
        let root = ENode { node: root.node.clone(), payload };
        Ok(STRoot::ENode(root, vec![]))
      }
      _ => Ok(STRoot::None),
    }
  }

  /// `i` 番目のエントリの位置を参照します。この検索は現在のルートノードを基準にした探索を行います。
  fn get_entry_position(
    &self,
    cursor: &mut Box<dyn Cursor>,
    i: Index,
    with_branch: bool,
  ) -> Result<Option<(Index, Vec<Node>)>> {
    match &self.root() {
      Root::INode(root) => {
        let root = (*root).clone();
        search_entry_position(cursor, &root, i, with_branch)
      }
      Root::ENode(root) if root.node.address.i == i => {
        Ok(Some((root.node.address.position, vec![])))
      }
      _ => Ok(None),
    }
  }
}

/// 指定されたカーソルの現在の位置からエントリを読み込みます。
/// 正常終了時のカーソルは次のエントリを指しています。
fn read_entry<C>(r: &mut C, i_expected: Index) -> Result<Entry>
  where
    C: io::Read + io::Seek,
{
  let position = r.stream_position()?;
  let mut hasher = HighwayBuilder::new(Key(CHECKSUM_HW64_KEY));
  let mut r = HashRead::new(r, &mut hasher);
  let entry = read_entry_without_check(&mut r, position, i_expected)?;

  // オフセットの検証
  let offset = r.length();
  let trailer_offset = r.read_u32::<LittleEndian>()?;
  if offset != trailer_offset as u64 {
    return Err(IncorrectEntryHeadOffset { expected: trailer_offset, actual: offset });
  }

  // チェックサムの検証
  let checksum = r.finish();
  let trailer_checksum = r.read_u64::<LittleEndian>()?;
  if checksum != trailer_checksum {
    let length = offset as u32 + 4 + 8;
    return Err(ChecksumVerificationFailed {
      at: position,
      length,
      expected: trailer_checksum,
      actual: checksum,
    });
  }

  Ok(entry)
}

/// 指定されたカーソルの現在の位置からエントリを読み込みます。トレイラーの offset と checksum は読み込まれない
/// ため、正常終了時のカーソルは offset の位置を指しています。
fn read_entry_without_check(r: &mut dyn io::Read, position: u64, i_expected: Index) -> Result<Entry> {
  let mut hash = [0u8; HASH_SIZE];

  // 中間ノードの読み込み
  let inodes = read_inodes(r, position)?;
  let i = inodes.first().map(|inode| inode.node.address.i).unwrap_or(1);
  if i != i_expected && i_expected != 0 {
    return Err(Detail::IncorrectNodeBoundary { at: position });
  }

  // 葉ノードの読み込み
  let payload_size = r.read_u32::<LittleEndian>()? & MAX_PAYLOAD_SIZE as u32;
  let mut payload = Vec::<u8>::with_capacity(payload_size as usize);
  unsafe { payload.set_len(payload_size as usize) };
  r.read_exact(&mut payload)?;
  r.read_exact(&mut hash)?;
  let enode = ENode { node: Node::new(Address::new(i, 0, position), Hash::new(hash)), payload };

  Ok(Entry { enode, inodes })
}

/// 指定されたカーソルの現在の位置をエントリの先頭としてすべての `INode` を読み込みます。正常終了した場合、カーソル
/// 位置は最後の `INode` を読み込んだ直後を指しています。
fn read_inodes(r: &mut dyn io::Read, position: u64) -> Result<Vec<INode>> {
  let mut hash = [0u8; HASH_SIZE];
  let i = r.read_u64::<LittleEndian>()?;
  let inode_count = r.read_u8()?;
  let mut right_j = 0u8;
  let mut inodes = Vec::<INode>::with_capacity(inode_count as usize);
  for _ in 0..inode_count as usize {
    let j = (r.read_u8()? & (INDEX_SIZE - 1)) + 1; // 下位 6-bit のみを使用
    let left_position = r.read_u64::<LittleEndian>()?;
    let left_i = r.read_u64::<LittleEndian>()?;
    let left_j = r.read_u8()?;
    r.read_exact(&mut hash)?;
    inodes.push(INode {
      node: Node::new(Address::new(i, j, position), Hash::new(hash)),
      left: Address::new(left_i, left_j, left_position),
      right: Address::new(i, right_j, position),
    });
    right_j = j;
  }
  Ok(inodes)
}

/// 指定されたカーソルにエントリを書き込みます。
/// このエントリに対して書き込みが行われた長さを返します。
fn write_entry(w: &mut dyn Write, e: &Entry) -> Result<usize> {
  debug_assert!(e.enode.payload.len() <= MAX_PAYLOAD_SIZE);
  debug_assert!(e.inodes.len() <= 0xFF);

  let mut hasher = HighwayBuilder::new(Key(CHECKSUM_HW64_KEY));
  let mut w = HashWrite::new(w, &mut hasher);

  // 中間ノードの書き込み
  w.write_u64::<LittleEndian>(e.enode.node.address.i)?;
  w.write_u8(e.inodes.len() as u8)?;
  for i in &e.inodes {
    debug_assert_eq!((i.node.address.j - 1) & (INDEX_SIZE - 1), i.node.address.j - 1);
    w.write_u8((i.node.address.j - 1) & (INDEX_SIZE - 1))?; // 下位 6-bit のみ保存
    w.write_u64::<LittleEndian>(i.left.position)?;
    w.write_u64::<LittleEndian>(i.left.i)?;
    w.write_u8(i.left.j)?;
    w.write_all(&i.node.hash.value)?;
  }

  // 葉ノードの書き込み
  w.write_u32::<LittleEndian>(e.enode.payload.len() as u32)?;
  w.write_all(&e.enode.payload)?;
  w.write_all(&e.enode.node.hash.value)?;

  // エントリ先頭までのオフセットを書き込み
  w.write_u32::<LittleEndian>(w.length() as u32)?;

  // チェックサムの書き込み
  w.write_u64::<LittleEndian>(w.finish())?;

  Ok(w.length() as usize)
}

/// `root` に指定された中間ノードを部分木構造のルートとして b_{i,*} に該当する葉ノードと中間ノードを含んでいる
/// エントリのストレージ内での位置を取得します。該当するエントリが存在しない場合は `None` を返します。
///
/// `with_branch` に true を指定した場合、返値には `root` から検索対象のノードに至るまでの分岐先のハッシュ値を
/// 持つノードが含まれます。これはハッシュツリーからハッシュ付きで値を参照するための動作です。false を指定した場合は
/// 長さ 0 の `Vec` を返します。
///
fn search_entry_position<C>(
  r: &mut C,
  root: &INode,
  i: Index,
  with_branch: bool,
) -> Result<Option<(u64, Vec<Node>)>>
  where
    C: io::Read + io::Seek,
{
  if root.node.address.i == i {
    // 指定されたルートノードが検索対象のノードの場合
    return Ok(Some((root.node.address.position, vec![])));
  } else if i == 0 {
    // インデックス 0 の特殊値を持つノードは明示的に存在しない
    return Ok(None);
  }

  let mut branches = Vec::<Node>::with_capacity(INDEX_SIZE as usize);
  let mut mover = root.clone();
  for _ in 0..INDEX_SIZE {
    // 次のノードのアドレスを参照
    let (next, branch) = if i <= mover.left.i {
      (mover.left, mover.right)
    } else if i <= mover.node.address.i {
      (mover.right, mover.left)
    } else {
      // 有効範囲外
      return Ok(None);
    };

    // 次のノードのアドレスが検索対象ならそのエントリの位置を返す
    if next.i == i {
      return Ok(Some((next.position, branches)));
    }

    // 末端に到達している場合は発見できなかったことを意味する
    if next.j == 0 {
      return Ok(None);
    }

    fn read_inode<C>(r: &mut C, addr: &Address) -> Result<INode>
      where
        C: io::Read + io::Seek,
    {
      debug_assert_ne!(0, addr.j);
      r.seek(io::SeekFrom::Start(addr.position))?;
      let inodes = read_inodes(r, addr.position)?;
      let inode = inodes.iter().find(|inode| inode.node.address.j == addr.j);
      if let Some(inode) = inode {
        Ok(inode.clone())
      } else {
        // 内部の木構造とストレージ上のデータが矛盾している
        inconsistency(format!(
          "entry i={} in storage doesn't contain an inode at specified level j={}",
          addr.i, addr.j
        ))
      }
    }

    // b_{i,*} の中間ノードをロードして次の中間ノードを取得
    mover = read_inode(r, &next)?;

    if with_branch {
      let branch = if branch.j == 0 {
        r.seek(io::SeekFrom::Start(branch.position))?;
        let entry = read_entry_without_check(r, branch.position, branch.i)?;
        entry.enode.node
      } else {
        read_inode(r, &branch)?.node
      };
      branches.push(branch);
    }
  }

  // ストレージ上のデータのポインタが循環参照を起こしている
  inconsistency(format!(
    "The maximum hop count was exceeded before reaching node b_{} from node b_{{{},{}}}.\
     The data on the storage probably have circular references.",
    i, root.node.address.i, root.node.address.j
  ))
}

/// 指定されたカーソルを現在の位置から `distance` バイト前方に移動します。移動先がカーソルの先頭を超える場合は
/// `if_err` をメッセージとしたエラーを発生します。
#[inline]
fn back_to_safety(cursor: &mut dyn Cursor, distance: u32, if_err: &'static str) -> Result<u64> {
  let from = cursor.stream_position()?;
  let to = from - distance as u64;
  if to < STORAGE_IDENTIFIER.len() as u64 + 1 {
    Err(DamagedStorage(format!("{} (cannot move position from {} to {})", if_err, from, to)))
  } else {
    Ok(cursor.seek(io::SeekFrom::Current(-(distance as i64)))?)
  }
}

fn inconsistency<T>(msg: String) -> Result<T> {
  #[cfg(feature = "panic_over_inconsistency")]
    {
      panic!("{}", msg)
    }
  #[cfg(not(feature = "panic_over_inconsistency"))]
    {
      Err(InternalStateInconsistency { message: msg })
    }
}

#[inline]
fn hex(value: &[u8]) -> String {
  value.iter().map(|c| format!("{:02X}", c)).collect()
}
