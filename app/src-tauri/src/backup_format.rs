//! The backup file format (`.konakbackup`), shared by the backup engine (writer) and the restore
//! engine (reader).
//!
//! Layout of a file:
//! - the plain-text magic line `KONAK-BACKUP\n` (13 bytes), so Konak can tell its own files without
//!   the password;
//! - an age v1 file (<https://age-encryption.org/v1>) with one scrypt passphrase stanza: the payload
//!   is ChaCha20-Poly1305 in 64 KiB chunks, each authenticated, the last one marked, so any changed
//!   byte or a truncation fails decryption at that point;
//! - inside it, a zstd stream; inside that, a tar stream (ustar/GNU headers) of regular files only,
//!   written in this order (files of earlier builds have the apps right after the manifest; readers
//!   take any order between the manifest and the checksums):
//!   - `manifest.json`, always the first entry (`Manifest`);
//!   - `konak/ca/root.crt`, `konak/ca/root.key`, `konak/state.json` (apps, rendered ports),
//!     `konak/settings.json` (never the password);
//!   - `photos/<relative path>`, `media/<relative path>`: the Windows folders' files, one entry each,
//!     before the apps, so a restore has them in place before it starts an app that mounts them;
//!   - `apps/<id>/folder/<NNNNNN>`: consecutive chunks (at most `CHUNK` bytes each) of GNU tar's
//!     output for the app's folder, `tar -C /var/lib/konak/apps -cpf - <id>` (members start with
//!     `<id>/`, numeric owners, xattrs, ACLs); concatenated in order, they are that tar stream;
//!   - `apps/<id>/volumes/<name>/<NNNNNN>`: the same for `podman volume export <podman_name>`;
//!   - `checksums.json`, always the last entry (`Checksums`): size and SHA-256 of every entry
//!     before it, in archive order (manifest included).
//!
//! The manifest comes first so it can be read without decrypting the whole file, which also means
//! it cannot hold checksums of data written after it; those come last. A reader therefore knows an
//! entry's data matched its checksum only once it reaches `checksums.json` (age has already
//! authenticated every byte by then). A restore must verify the whole file (`verify`) before it
//! changes anything, or stage what it extracts until `Backup::for_each_entry` returns `Ok`.

use age::secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

pub const MAGIC: &[u8] = b"KONAK-BACKUP\n";
/// The format this build writes and the newest it reads.
pub const FORMAT: u32 = 1;
/// Distro streams are cut into entries of at most this many bytes.
pub const CHUNK: usize = 16 << 20;
/// scrypt work factor for new files, `N = 2^18` (age's reference implementation uses the same).
pub const SCRYPT_LOG_N: u8 = 18;
/// The highest work factor a reader accepts (`N = 2^20`, about 1 GiB of memory), which bounds what
/// a crafted file can cost.
const MAX_LOG_N: u8 = 20;
const MANIFEST: &str = "manifest.json";
const CHECKSUMS: &str = "checksums.json";
const MAX_MANIFEST: u64 = 16 << 20;
const MAX_CHECKSUMS: u64 = 512 << 20;
const ZSTD_LEVEL: i32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub format: u32,
    pub konak_version: String,
    pub created_unix: u64,
    pub apps: Vec<AppManifest>,
    /// Estimated size of the Photos folder at the start of the backup; `None` when not included.
    pub photos_bytes: Option<u64>,
    pub media_bytes: Option<u64>,
    /// Which `konak/...` entries the file has (`ca/root.crt`, `ca/root.key`, `state.json`, `settings.json`).
    #[serde(default)]
    pub konak_files: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppManifest {
    pub id: String,
    pub name: String,
    pub claimed: bool,
    pub stopped: bool,
    /// Estimated size of the app's folder and volumes, measured before it was archived.
    pub size_bytes: u64,
    #[serde(default)]
    pub volumes: Vec<VolumeManifest>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VolumeManifest {
    /// The compose name (`db`), as in the app's compose.yml; the entry path uses it.
    pub name: String,
    /// The Podman volume name (`nextcloud_db`), for `podman volume import`.
    pub podman_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Checksums {
    pub entries: Vec<EntrySum>,
    /// Windows-folder files left out (unreadable, or a name that is not valid Unicode), and files
    /// that shrank while they were read (their entry is zero-padded to the size first seen).
    #[serde(default)]
    pub skipped: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EntrySum {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// What an entry path stands for. Paths that parse to none of these are refused by the reader, so
/// a restore never sees `..`, absolute paths or other surprises.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Entry {
    Manifest,
    AppFolder { app: String, index: u32 },
    AppVolume { app: String, volume: String, index: u32 },
    /// A path under `konak/`, e.g. `ca/root.key`.
    Konak(String),
    /// A relative path, `/`-separated.
    Photos(String),
    Media(String),
    Checksums,
}

fn label_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn volume_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) && !s.starts_with('.')
}

/// A relative path made of ordinary components only.
fn relative_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains('\\') && !s.contains('\0') && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..") && !s.contains(':')
}

fn index_of(s: &str) -> Option<u32> {
    (s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

pub fn chunk_name(index: u32) -> String {
    format!("{index:06}")
}

pub fn parse_entry(path: &str) -> Option<Entry> {
    if path == MANIFEST {
        return Some(Entry::Manifest);
    }
    if path == CHECKSUMS {
        return Some(Entry::Checksums);
    }
    let (top, rest) = path.split_once('/')?;
    match top {
        "apps" => {
            let parts: Vec<&str> = rest.split('/').collect();
            match parts[..] {
                [app, "folder", i] if label_ok(app) => Some(Entry::AppFolder { app: app.into(), index: index_of(i)? }),
                [app, "volumes", v, i] if label_ok(app) && volume_ok(v) => Some(Entry::AppVolume { app: app.into(), volume: v.into(), index: index_of(i)? }),
                _ => None,
            }
        }
        "konak" if relative_ok(rest) => Some(Entry::Konak(rest.into())),
        "photos" if relative_ok(rest) => Some(Entry::Photos(rest.into())),
        "media" if relative_ok(rest) => Some(Entry::Media(rest.into())),
        _ => None,
    }
}

// ---- Writer --------------------------------------------------------------------------------------

/// Counts the bytes written through it (the uncompressed tar stream).
struct Counting<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

type Inner<W> = Counting<zstd::Encoder<'static, age::stream::StreamWriter<W>>>;

/// Streams a backup file: nothing is staged on disk, distro streams are held one chunk at a time.
pub struct Writer<W: Write> {
    tar: tar::Builder<Inner<W>>,
    sums: Vec<EntrySum>,
    skipped: Vec<String>,
}

fn header(size: u64, mode: u32, mtime: u64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(size);
    h.set_mode(mode);
    h.set_mtime(mtime);
    h
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hashes what is read through it; after an early end of the source it yields zeros up to `size`
/// (and says so), so the tar header's size always holds.
struct Exact<R> {
    inner: R,
    left: u64,
    hash: Sha256,
    short: bool,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.left as usize);
        let n = if self.short { 0 } else { self.inner.read(&mut buf[..want])? };
        let n = if n == 0 {
            self.short = true;
            buf[..want].fill(0);
            want
        } else {
            n
        };
        self.hash.update(&buf[..n]);
        self.left -= n as u64;
        Ok(n)
    }
}

impl<W: Write> Writer<W> {
    /// Writes the magic line and the age header to `out`, then `manifest` as the first entry.
    /// `log_n` is the scrypt work factor (`SCRYPT_LOG_N`; tests use less).
    pub fn new(mut out: W, password: &SecretString, manifest: &Manifest, log_n: u8) -> io::Result<Self> {
        out.write_all(MAGIC)?;
        let mut recipient = age::scrypt::Recipient::new(password.clone());
        recipient.set_work_factor(log_n);
        let enc = age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient)).map_err(io::Error::other)?;
        let age = enc.wrap_output(out)?;
        let zstd = zstd::Encoder::new(age, ZSTD_LEVEL)?;
        let mut w = Writer { tar: tar::Builder::new(Counting { inner: zstd, count: 0 }), sums: vec![], skipped: vec![] };
        let body = serde_json::to_vec_pretty(manifest)?;
        w.append(MANIFEST, body.len() as u64, body.as_slice(), 0o600, manifest.created_unix)?;
        Ok(w)
    }

    /// Uncompressed bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.tar.get_ref().count
    }

    /// Appends exactly `size` bytes read from `data` as `path`. Returns false when `data` ended early
    /// (the rest was zero-filled and the path is listed in `skipped`). The manifest and the
    /// checksums are written by `new` and `finish` only.
    pub fn add_reader(&mut self, path: &str, size: u64, data: impl Read, mode: u32, mtime: u64) -> io::Result<bool> {
        match parse_entry(path) {
            Some(Entry::Manifest | Entry::Checksums) | None => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a valid entry path: {path}"))),
            Some(_) => self.append(path, size, data, mode, mtime),
        }
    }

    fn append(&mut self, path: &str, size: u64, data: impl Read, mode: u32, mtime: u64) -> io::Result<bool> {
        let mut exact = Exact { inner: data, left: size, hash: Sha256::new(), short: false };
        let mut h = header(size, mode, mtime);
        self.tar.append_data(&mut h, path, &mut exact)?;
        if exact.short {
            self.skipped.push(format!("{path} (changed while it was read)"));
        }
        self.sums.push(EntrySum { path: path.into(), size, sha256: hex(&exact.hash.finalize()) });
        Ok(!exact.short)
    }

    pub fn add_bytes(&mut self, path: &str, data: &[u8], mode: u32, mtime: u64) -> io::Result<()> {
        self.add_reader(path, data.len() as u64, data, mode, mtime).map(|_| ())
    }

    /// Appends a stream of unknown length as consecutive chunks `<prefix>/000000`, `000001`, ...
    /// `progress` hears the total uncompressed bytes after each chunk. Returns the stream's length.
    pub fn add_stream(&mut self, prefix: &str, mut data: impl Read, mtime: u64, progress: &mut dyn FnMut(u64)) -> io::Result<u64> {
        let mut buf = vec![0u8; CHUNK];
        let (mut index, mut total) = (0u32, 0u64);
        loop {
            let mut filled = 0;
            while filled < CHUNK {
                match data.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if filled == 0 {
                return Ok(total);
            }
            self.add_bytes(&format!("{prefix}/{}", chunk_name(index)), &buf[..filled], 0o600, mtime)?;
            total += filled as u64;
            index += 1;
            progress(self.bytes());
            if filled < CHUNK {
                return Ok(total);
            }
        }
    }

    /// Records a left-out file in `checksums.json`.
    pub fn skip(&mut self, what: String) {
        self.skipped.push(what);
    }

    /// Writes `checksums.json`, ends the tar, zstd and age streams, and returns `out` (unflushed
    /// beyond what the streams wrote; a caller with a file syncs it).
    pub fn finish(mut self, mtime: u64) -> io::Result<(W, Checksums)> {
        let sums = Checksums { entries: std::mem::take(&mut self.sums), skipped: std::mem::take(&mut self.skipped) };
        let body = serde_json::to_vec(&sums)?;
        let mut h = header(body.len() as u64, 0o600, mtime);
        self.tar.append_data(&mut h, CHECKSUMS, body.as_slice())?;
        let counting = self.tar.into_inner()?;
        let age = counting.inner.finish()?;
        Ok((age.finish()?, sums))
    }
}

// ---- Reader --------------------------------------------------------------------------------------

/// Reader errors are short codes (locale keys under `errors.`); details go to the caller's log
/// through `detail`.
#[derive(Debug, PartialEq)]
pub struct ReadError {
    pub code: &'static str,
    pub detail: String,
}

impl ReadError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        ReadError { code, detail: detail.into() }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

fn damaged(e: impl std::fmt::Display) -> ReadError {
    ReadError::new("backup_damaged", e.to_string())
}

type Plain = zstd::Decoder<'static, BufReader<age::stream::StreamReader<BufReader<std::fs::File>>>>;

/// An opened backup: its manifest, and the rest of the archive to walk once.
pub struct Backup {
    pub manifest: Manifest,
    manifest_sum: EntrySum,
    rest: tar::Archive<Plain>,
}

/// Whether `path` starts with Konak's magic line (no password needed).
pub fn has_magic(path: &Path) -> bool {
    let mut buf = [0u8; MAGIC.len()];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut buf)).is_ok() && buf == MAGIC
}

/// Opens a backup file and reads its manifest. Errors: `backup_not_konak` (not a Konak backup),
/// `backup_wrong_password`, `backup_newer_format` (made by a newer Konak), `backup_damaged`.
pub fn open(path: &Path, password: &SecretString) -> Result<Backup, ReadError> {
    let mut file = BufReader::new(std::fs::File::open(path).map_err(|e| ReadError::new("backup_unreadable", e.to_string()))?);
    let mut magic = [0u8; MAGIC.len()];
    if file.read_exact(&mut magic).is_err() || magic != MAGIC {
        return Err(ReadError::new("backup_not_konak", "no Konak magic line"));
    }
    let dec = age::Decryptor::new(file).map_err(|e| ReadError::new("backup_not_konak", e.to_string()))?;
    if !dec.is_scrypt() {
        return Err(ReadError::new("backup_not_konak", "not passphrase-encrypted"));
    }
    let mut identity = age::scrypt::Identity::new(password.clone());
    identity.set_max_work_factor(MAX_LOG_N);
    let plain = dec.decrypt(std::iter::once(&identity as &dyn age::Identity)).map_err(|e| match e {
        age::DecryptError::DecryptionFailed | age::DecryptError::NoMatchingKeys => ReadError::new("backup_wrong_password", e.to_string()),
        e => damaged(e),
    })?;
    let mut plain = zstd::Decoder::new(plain).map_err(damaged)?;
    let (manifest, manifest_sum) = read_manifest(&mut plain)?;
    Ok(Backup { manifest, manifest_sum, rest: tar::Archive::new(plain) })
}

/// Reads the first tar entry by hand (header, data, padding), so the rest of the stream can go to
/// a `tar::Archive` that starts at the next header.
fn read_manifest(r: &mut impl Read) -> Result<(Manifest, EntrySum), ReadError> {
    let mut block = [0u8; 512];
    r.read_exact(&mut block).map_err(damaged)?;
    let h = tar::Header::from_byte_slice(&block);
    let path = h.path().map_err(damaged)?.to_string_lossy().to_string();
    let size = h.size().map_err(damaged)?;
    if path != MANIFEST || h.entry_type() != tar::EntryType::Regular || size > MAX_MANIFEST {
        return Err(damaged(format!("first entry is {path}, not the manifest")));
    }
    if h.cksum().map_err(damaged)? != tar_checksum(&block) {
        return Err(damaged("manifest header checksum"));
    }
    let mut data = vec![0u8; size as usize];
    r.read_exact(&mut data).map_err(damaged)?;
    let pad = (512 - size % 512) % 512;
    io::copy(&mut r.take(pad), &mut io::sink()).map_err(damaged)?;
    let version: serde_json::Value = serde_json::from_slice(&data).map_err(damaged)?;
    match version.get("format").and_then(|f| f.as_u64()) {
        Some(f) if f > FORMAT as u64 => return Err(ReadError::new("backup_newer_format", format!("format {f}, this Konak reads up to {FORMAT}"))),
        Some(_) => {}
        None => return Err(damaged("manifest without a format")),
    }
    let manifest: Manifest = serde_json::from_slice(&data).map_err(damaged)?;
    let sum = EntrySum { path: MANIFEST.into(), size, sha256: hex(&Sha256::digest(&data)) };
    Ok((manifest, sum))
}

/// The ustar header checksum: the header's bytes summed with the checksum field read as spaces.
fn tar_checksum(block: &[u8; 512]) -> u32 {
    block.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as u32 }).sum()
}

/// Hashes everything read through it.
struct Hashing<'a, R> {
    inner: &'a mut R,
    hash: Sha256,
    count: u64,
}

impl<R: Read> Read for Hashing<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

impl Backup {
    /// Walks every entry after the manifest, in archive order. `f` gets each entry (never the
    /// checksums) and may read any part of its data; the rest is read and hashed here. Returns the
    /// checksums once every entry matched them; any mismatch, a missing or misplaced
    /// `checksums.json`, an unknown entry path or a non-file entry is `backup_damaged`.
    pub fn for_each_entry(mut self, mut f: impl FnMut(&Entry, &str, &mut dyn Read) -> Result<(), String>) -> Result<Checksums, ReadError> {
        let mut seen = vec![self.manifest_sum.clone()];
        let mut checksums: Option<Checksums> = None;
        for entry in self.rest.entries().map_err(damaged)? {
            let mut entry = entry.map_err(damaged)?;
            if checksums.is_some() {
                return Err(damaged("entries after checksums.json"));
            }
            let path = entry.path().map_err(damaged)?.to_str().ok_or_else(|| damaged("entry path is not Unicode"))?.to_string();
            if entry.header().entry_type() != tar::EntryType::Regular {
                return Err(damaged(format!("{path} is not a regular file")));
            }
            let kind = parse_entry(&path).ok_or_else(|| damaged(format!("unexpected entry {path}")))?;
            match kind {
                Entry::Manifest => return Err(damaged("a second manifest")),
                Entry::Checksums => {
                    if entry.size() > MAX_CHECKSUMS {
                        return Err(damaged("checksums.json too large"));
                    }
                    let mut body = vec![];
                    entry.read_to_end(&mut body).map_err(damaged)?;
                    checksums = Some(serde_json::from_slice(&body).map_err(damaged)?);
                }
                kind => {
                    let mut h = Hashing { inner: &mut entry, hash: Sha256::new(), count: 0 };
                    f(&kind, &path, &mut h).map_err(|e| ReadError::new("backup_restore_failed", e))?;
                    io::copy(&mut h, &mut io::sink()).map_err(damaged)?;
                    seen.push(EntrySum { path, size: h.count, sha256: hex(&h.hash.finalize()) });
                }
            }
        }
        let sums = checksums.ok_or_else(|| damaged("no checksums.json (file cut short)"))?;
        if sums.entries != seen {
            let first = sums.entries.iter().zip(&seen).find(|(a, b)| a != b).map(|(a, _)| a.path.clone());
            return Err(damaged(format!("checksums do not match ({} listed, {} read, first difference {first:?})", sums.entries.len(), seen.len())));
        }
        Ok(sums)
    }
}

/// Reads the whole file: decrypts it and checks every entry against its checksum.
pub fn verify(path: &Path, password: &SecretString) -> Result<(Manifest, Checksums), ReadError> {
    let backup = open(path, password)?;
    let manifest = backup.manifest.clone();
    let sums = backup.for_each_entry(|_, _, _| Ok(()))?;
    Ok((manifest, sums))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const TEST_LOG_N: u8 = 10;

    pub fn pw(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    pub fn manifest(created: u64) -> Manifest {
        Manifest {
            format: FORMAT,
            konak_version: "0.1.0".into(),
            created_unix: created,
            apps: vec![AppManifest {
                id: "nextcloud".into(),
                name: "cloud".into(),
                claimed: true,
                stopped: false,
                size_bytes: 10,
                volumes: vec![VolumeManifest { name: "db".into(), podman_name: "nextcloud_db".into() }],
            }],
            photos_bytes: Some(3),
            media_bytes: None,
            konak_files: vec!["ca/root.crt".into()],
        }
    }

    /// Writes a small backup to `path`; `stream` becomes `apps/nextcloud/volumes/db/...`.
    pub fn write_sample(path: &Path, password: &str, m: &Manifest, stream: &[u8]) -> Checksums {
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut w = Writer::new(file, &pw(password), m, TEST_LOG_N).unwrap();
        w.add_stream("apps/nextcloud/folder", &b"folder tar bytes"[..], 1, &mut |_| {}).unwrap();
        w.add_stream("apps/nextcloud/volumes/db", stream, 1, &mut |_| {}).unwrap();
        w.add_bytes("konak/ca/root.crt", b"-----BEGIN CERTIFICATE-----", 0o600, 1).unwrap();
        assert!(w.add_reader("photos/2026/a.jpg", 3, &b"jpg"[..], 0o644, 1).unwrap());
        // A file that shrank after its size was taken: padded, and listed as skipped.
        assert!(!w.add_reader("photos/2026/b.jpg", 4, &b"ab"[..], 0o644, 1).unwrap());
        let (mut file, sums) = w.finish(1).unwrap();
        file.flush().unwrap();
        sums
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("konak-format-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip() {
        let dir = temp("round");
        let path = dir.join("b.konakbackup");
        // More than one chunk, so the chunking is exercised.
        let big: Vec<u8> = (0..CHUNK + 1000).map(|i| (i % 251) as u8).collect();
        let written = write_sample(&path, "the backup password", &manifest(42), &big);
        assert!(has_magic(&path));
        let b = open(&path, &pw("the backup password")).unwrap();
        assert_eq!(b.manifest, manifest(42));
        let mut seen = vec![];
        let mut volume = vec![];
        let sums = b
            .for_each_entry(|e, p, r| {
                seen.push((e.clone(), p.to_string()));
                if let Entry::AppVolume { .. } = e {
                    r.read_to_end(&mut volume).map_err(|e| e.to_string())?;
                }
                Ok(())
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(sums, written);
        assert_eq!(volume, big, "the chunks concatenate to the stream");
        let kinds: Vec<Entry> = seen.into_iter().map(|(e, _)| e).collect();
        assert_eq!(
            kinds,
            [
                Entry::AppFolder { app: "nextcloud".into(), index: 0 },
                Entry::AppVolume { app: "nextcloud".into(), volume: "db".into(), index: 0 },
                Entry::AppVolume { app: "nextcloud".into(), volume: "db".into(), index: 1 },
                Entry::Konak("ca/root.crt".into()),
                Entry::Photos("2026/a.jpg".into()),
                Entry::Photos("2026/b.jpg".into()),
            ]
        );
        assert_eq!(sums.skipped, ["photos/2026/b.jpg (changed while it was read)"]);
        assert_eq!(sums.entries[0].path, "manifest.json");
    }

    #[test]
    fn the_file_is_unreadable_without_the_password() {
        let dir = temp("pw");
        let path = dir.join("b.konakbackup");
        write_sample(&path, "the backup password", &manifest(1), b"secret volume data");
        let raw = std::fs::read(&path).unwrap();
        let wrong = open(&path, &pw("not the password")).err().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(wrong.code, "backup_wrong_password");
        assert!(!raw.windows(6).any(|w| w == b"secret") && !raw.windows(9).any(|w| w == b"nextcloud"), "nothing in plain text");
        assert!(raw.starts_with(b"KONAK-BACKUP\nage-encryption.org/v1\n-> scrypt "));
    }

    #[test]
    fn a_wrong_password_is_refused_at_the_age_header() {
        // Only the magic line, the age header and the payload nonce are left: a wrong password is
        // told from the scrypt stanza alone, without any payload, and the right one meets the
        // missing payload.
        let dir = temp("header");
        let path = dir.join("b.konakbackup");
        write_sample(&path, "the backup password", &manifest(1), b"data");
        let raw = std::fs::read(&path).unwrap();
        let end = raw.windows(4).position(|w| w == b"\n---").unwrap();
        let header_end = end + raw[end + 1..].iter().position(|&b| b == b'\n').unwrap() + 2;
        std::fs::write(&path, &raw[..header_end + 16]).unwrap();
        let wrong = open(&path, &pw("not the password")).err().unwrap().code;
        let right = open(&path, &pw("the backup password")).err().unwrap().code;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(wrong, "backup_wrong_password");
        assert_eq!(right, "backup_damaged");
    }

    #[test]
    fn a_changed_byte_or_a_cut_is_detected() {
        let dir = temp("tamper");
        let path = dir.join("b.konakbackup");
        let data: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
        write_sample(&path, "the backup password", &manifest(1), &data);
        let good = std::fs::read(&path).unwrap();
        let check = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            verify(&path, &pw("the backup password")).err().map(|e| e.code)
        };
        let mut flipped = good.clone();
        let mid = good.len() / 2;
        flipped[mid] ^= 0x01;
        let mut last = good.clone();
        *last.last_mut().unwrap() ^= 0x80;
        let results = [check(&good), check(&flipped), check(&last), check(&good[..good.len() - 100]), check(&good[..good.len() / 3])];
        let mut not_konak = good.clone();
        not_konak[0] = b'X';
        let wrong_magic = check(&not_konak);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(results[0], None);
        for r in &results[1..] {
            assert_eq!(*r, Some("backup_damaged"));
        }
        assert_eq!(wrong_magic, Some("backup_not_konak"));
    }

    #[test]
    fn a_checksum_mismatch_is_detected() {
        // A writer bug (or a file put together from two backups): the age layer is intact, but the
        // list does not match the data.
        let dir = temp("sums");
        let path = dir.join("b.konakbackup");
        let file = std::fs::File::create(&path).unwrap();
        let mut w = Writer::new(file, &pw("the backup password"), &manifest(1), TEST_LOG_N).unwrap();
        w.add_bytes("konak/state.json", b"{}", 0o600, 1).unwrap();
        w.sums[1].sha256 = "00".repeat(32);
        w.finish(1).unwrap();
        let r = verify(&path, &pw("the backup password")).err().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.code, "backup_damaged");
        assert!(r.detail.contains("konak/state.json"), "{}", r.detail);
    }

    #[test]
    fn a_newer_format_is_refused() {
        let dir = temp("newer");
        let path = dir.join("b.konakbackup");
        let m = Manifest { format: FORMAT + 1, ..manifest(1) };
        write_sample(&path, "the backup password", &m, b"x");
        let r = open(&path, &pw("the backup password")).err().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.code, "backup_newer_format");
    }

    #[test]
    fn entry_paths_are_strict() {
        assert_eq!(parse_entry("apps/immich/folder/000012"), Some(Entry::AppFolder { app: "immich".into(), index: 12 }));
        assert_eq!(parse_entry("apps/nextcloud/volumes/nextcloud/000000"), Some(Entry::AppVolume { app: "nextcloud".into(), volume: "nextcloud".into(), index: 0 }));
        assert_eq!(parse_entry("media/Films/a b.mkv"), Some(Entry::Media("Films/a b.mkv".into())));
        for bad in ["", "x", "apps/Immich/folder/000000", "apps/immich/folder/12", "apps/immich/other/000000", "photos/../x", "photos//x", "photos/a\\b", "photos/C:x", "media/", "konak/./ca", "apps/a/volumes/.x/000000", "other/x"] {
            assert_eq!(parse_entry(bad), None, "{bad}");
        }
        let file = std::fs::File::create(std::env::temp_dir().join(format!("konak-format-paths-{}", std::process::id()))).unwrap();
        let mut w = Writer::new(file, &pw("the backup password"), &manifest(1), TEST_LOG_N).unwrap();
        assert!(w.add_bytes("photos/../evil", b"x", 0o600, 1).is_err());
        assert!(w.add_bytes("checksums.json", b"x", 0o600, 1).is_err());
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("konak-format-paths-{}", std::process::id())));
    }
}
