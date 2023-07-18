use fastly::{Request, Response};
use fastly::kv_store::KVStore;
use fastly::mime::Mime;

use std::time::{Duration, Instant};
use std::io::ErrorKind;

use sqlite_vfs::{DatabaseHandle, LockKind, OpenKind, OpenOptions, Vfs, WalDisabled};

pub fn main() {
    eprintln!("version: {}", std::env::var("FASTLY_SERVICE_VERSION").unwrap());
    let r = Request::from_client();
    let start = std::time::Instant::now();

    sqlite_vfs::register("memvfs", InMemVfs {}, true).unwrap();

    let db = rusqlite::Connection::open_with_flags_and_vfs(
        "state.db",
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        "memvfs",
    ).expect("can open db");

    let resp_body: String = handle_req(db, &r).unwrap();

    let mut resp = Response::new();
    resp.set_body(resp_body);
//    resp.set_content_type(Mime::HTML);
// associated item not found in `Mime` ???
    resp.set_header("content-type", "text/html");
    resp.set_header("version", std::env::var("FASTLY_SERVICE_VERSION").unwrap());
    resp.set_header("host", std::env::var("FASTLY_HOSTNAME").unwrap());
    resp.set_header("render-time", start.elapsed().as_micros().to_string());

    resp.send_to_client()
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
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn sync(&mut self, data_only: bool) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn set_len(&mut self, size: u64) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(ErrorKind::Other, "no can do"))
    }

    fn lock(&mut self, lock: sqlite_vfs::LockKind) -> Result<bool, std::io::Error> {
        Ok(true)
    }

    fn reserved(&mut self) -> Result<bool, std::io::Error> {
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
