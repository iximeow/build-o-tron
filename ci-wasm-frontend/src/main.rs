use fastly::{Request, Response};
use fastly::kv_store::KVStore;
use fastly::mime::Mime;

use std::time::{Duration, Instant};
use std::io::ErrorKind;

use sqlite_vfs::{DatabaseHandle, LockKind, OpenKind, OpenOptions, Vfs, WalDisabled};

pub fn main() {
    eprintln!("version: {}", std::env::var("FASTLY_SERVICE_VERSION").unwrap());
    let mut r = Request::from_client();
    let start = std::time::Instant::now();

    let qs = r.get_query_str();
    let mut registered = false;
    let mut vfs = "none?";

    let mut resp = Response::new();

    let register_start = std::time::Instant::now();

    if let Some(qs) = qs {
        resp.set_header("qs", qs);
        if qs.ends_with("vfs=fd") {
            sqlite_vfs::register("memvfs", FdVfs {}, true).unwrap();
            vfs = "fd";
            registered = true;
        }
    }

    if !registered {
        sqlite_vfs::register("memvfs", InMemVfs {}, true).unwrap();
        vfs = "inmem";
    }

    let register_done = register_start.elapsed();

    let db_start = std::time::Instant::now();

    let db = match rusqlite::Connection::open_with_flags_and_vfs(
        "state.db",
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        "memvfs",
    ) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("got error {} at {}",
                e,
                std::env::var("FASTLY_HOSTNAME").unwrap()
            );
            panic!();
            return;
        }
    };

    let db_done = db_start.elapsed();

    let handle_start = std::time::Instant::now();

    let resp_body: String = handle_req(db, &r).unwrap();

    resp.set_body(resp_body);
//    resp.set_content_type(Mime::HTML);
// associated item not found in `Mime` ???
    resp.set_header("content-type", "text/html");
    resp.set_header("version", std::env::var("FASTLY_SERVICE_VERSION").unwrap());
    resp.set_header("host", std::env::var("FASTLY_HOSTNAME").unwrap());
    resp.set_header("total-time", start.elapsed().as_micros().to_string());
    resp.set_header("render-time", handle_start.elapsed().as_micros().to_string());
    resp.set_header("register-time", register_done.as_micros().to_string());
    resp.set_header("db-time", db_done.as_micros().to_string());
    resp.set_header("vfs", vfs);

    resp.send_to_client()
}

use fastly_shared::FastlyStatus;
#[link(wasm_import_module = "fastly_object_store")]
extern "C" {
    #[link_name = "lookup_as_fd"]
    pub fn lookup_as_fd(
        kv_store_handle: fastly_sys::KVStoreHandle,
        key_ptr: *const u8,
        key_len: usize,
        fd_out: *mut std::ffi::c_int,
    ) -> FastlyStatus;
}

use std::os::wasi::prelude::RawFd;
fn lookup_fd(key: &str) -> Result<RawFd, String> {
    let store = KVStore::open("ci-state")
        .map_err(|e| format!("could not open store ci-state: {:?}", e))?
        .ok_or_else(|| "ci-state is not an existing store")?;
    let mut db_fd: std::ffi::c_int = -1;
    let res = unsafe { lookup_as_fd(store.as_handle().as_u32(), key.as_bytes().as_ptr(), key.as_bytes().len(), &mut db_fd) };
    if res != FastlyStatus::OK {
        return Err(format!("bad fastly status: {:?}", res));
    }

    Ok(db_fd as RawFd)
}

struct InMemVfsHandle {
    bytes: Vec<u8>
}

impl DatabaseHandle for InMemVfsHandle {
    type WalIndex = WalDisabled;

    fn size(&self) -> Result<u64, std::io::Error> {
        Ok(self.bytes.len() as u64)
    }

    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<(), std::io::Error> {
        buf.copy_from_slice(&self.bytes[offset as usize..][..buf.len()]);
        Ok(())
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<(), std::io::Error> {
        eprintln!("write_all_at {}", offset);
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn sync(&mut self, data_only: bool) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn set_len(&mut self, size: u64) -> Result<(), std::io::Error> {
        eprintln!("set_len {}", size);
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn lock(&mut self, lock: sqlite_vfs::LockKind) -> Result<bool, std::io::Error> {
        eprintln!("lock {:?}", lock);
        Ok(true)
    }

    fn reserved(&mut self) -> Result<bool, std::io::Error> {
        eprintln!("reserved?");
        Ok(false)
    }

    fn current_lock(&self) -> Result<sqlite_vfs::LockKind, std::io::Error> {
        eprintln!("current_lock");
        Ok(sqlite_vfs::LockKind::None)

    }

    fn wal_index(&self, readonly: bool) -> Result<WalDisabled, std::io::Error> {
        eprintln!("wal");
        Ok(WalDisabled::default())
    }

    fn set_chunk_size(&self, chunk_size: usize) -> Result<(), std::io::Error> {
        eprintln!("set chunk size {}", chunk_size);
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }
}

struct FdVfsHandle {
    fd: RawFd,
}

impl DatabaseHandle for FdVfsHandle {
    type WalIndex = WalDisabled;

    fn size(&self) -> Result<u64, std::io::Error> {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let f = unsafe { std::fs::File::from_raw_fd(self.fd) };
        let len = f.metadata().unwrap().len();
        f.into_raw_fd();
        Ok(len)
    }

    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<(), std::io::Error> {
        let mut remaining = buf;
        let mut total_read = 0;
        let mut to_read = remaining.len();
        let mut file_offs = offset;
        while total_read < to_read {
            let n_read = unsafe {
                libc::pread(
                    self.fd,
                    remaining.as_mut_ptr() as *mut std::ffi::c_void,
                    remaining.len(),
                    file_offs as i64,
                )
            };
            if n_read == 0 {
                panic!("stopped reading?");
            }
            if n_read < 0 {
                panic!("read err: {}", n_read);
            }
            let n_read = n_read as usize;
            total_read += n_read;
            remaining = &mut remaining[(n_read as usize)..];
        }

        Ok(())

    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<(), std::io::Error> {
        eprintln!("write requested");
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn sync(&mut self, data_only: bool) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn set_len(&mut self, size: u64) -> Result<(), std::io::Error> {
        eprintln!("set len {}", size);
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn lock(&mut self, lock: sqlite_vfs::LockKind) -> Result<bool, std::io::Error> {

        eprintln!("lock {:?}", lock);
        Ok(true)
    }

    fn reserved(&mut self) -> Result<bool, std::io::Error> {
        eprintln!("reserved requested");
        Ok(false)
    }

    fn current_lock(&self) -> Result<sqlite_vfs::LockKind, std::io::Error> {
        Ok(sqlite_vfs::LockKind::None)

    }

    fn wal_index(&self, readonly: bool) -> Result<WalDisabled, std::io::Error> {
        Ok(WalDisabled::default())
    }

    fn set_chunk_size(&self, chunk_size: usize) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }
}

struct InMemVfs;

impl Vfs for InMemVfs {
    type Handle = InMemVfsHandle;

    fn open(&self, db: &str, opts: OpenOptions) -> Result<Self::Handle, std::io::Error> {
        if opts.kind != OpenKind::MainDb {
            return Err(std::io::Error::new(ErrorKind::Other, "no"));
        }

        let store = KVStore::open("ci-state")
            .expect("can open ci-state")
            .expect("ci-state exists");

        let bytes = store.lookup_bytes(db).expect("lookup_works").expect("bytes exist");

        Ok(InMemVfsHandle { bytes })
//         Ok(FdVfsHandle { fd })
    }

    fn delete(&self, _db: &str) -> Result<(), std::io::Error> {
        // do nothing for deletes
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn exists(&self, db: &str) -> Result<bool, std::io::Error> {
        Ok(false)
    }

    fn temporary_name(&self) -> String {
        "temp name".to_string()
    }

    fn random(&self, buffer: &mut [i8]) {
        rand::Rng::fill(&mut rand::thread_rng(), buffer);
    }

    fn sleep(&self, duration: Duration) -> Duration {
        let now = Instant::now();
        std::thread::sleep(duration);
        now.elapsed()
    }
}
struct FdVfs;

impl Vfs for FdVfs {
    type Handle = FdVfsHandle;

    fn open(&self, db: &str, opts: OpenOptions) -> Result<Self::Handle, std::io::Error> {
        if opts.kind != OpenKind::MainDb {
            return Err(std::io::Error::new(ErrorKind::Other, "no"));
        }

        let store = KVStore::open("ci-state")
            .expect("can open ci-state")
            .expect("ci-state exists");

        let fd = lookup_fd(db).map_err(|msg| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                msg,
            )
        })?;

//         Ok(InMemVfsHandle { fd })
         Ok(FdVfsHandle { fd })
    }

    fn delete(&self, _db: &str) -> Result<(), std::io::Error> {
        // do nothing for deletes
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn exists(&self, db: &str) -> Result<bool, std::io::Error> {
        Ok(false)
    }

    fn temporary_name(&self) -> String {
        "temp name".to_string()
    }

    fn random(&self, buffer: &mut [i8]) {
        rand::Rng::fill(&mut rand::thread_rng(), buffer);
    }

    fn sleep(&self, duration: Duration) -> Duration {
        let now = Instant::now();
        std::thread::sleep(duration);
        now.elapsed()
    }
}

pub fn handle_req(db: rusqlite::Connection, req: &Request) -> Result<String, String> {
    println!("hi");

    use ci_lib_core::dbctx::DbCtx;
    use std::sync::Arc;
    use std::sync::Mutex;

    let ctx = Arc::new(DbCtx {
        config_path: "/".into(),
        conn: Mutex::new(db),
    });

    if req.get_url().path() == "/" {
        ci_lib_web::build_repo_index(&ctx)
    } else {
        Ok("a".to_string())
    }
}
