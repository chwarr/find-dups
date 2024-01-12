use crossbeam::channel::unbounded;
use sha2::{Digest, Sha256};
use std::convert::AsRef;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::panic;
use std::path;
use std::thread;

type Sha256Sum = [u8; 32];

struct WorkResult {
    pub path: path::PathBuf,
    pub result: io::Result<Sha256Sum>,
}

fn main() -> io::Result<()> {
    let (results_sender, results_receiver) = unbounded();

    let worker_thread = thread::spawn(move || {
        for argument in env::args().skip(1) {
            let r = fingerprint_one_file(path::Path::new(&argument));
            results_sender
                .send(r)
                .expect("Unable to enqueue result into result channel");
        }
    });

    for result in results_receiver.iter() {
        if result.result.is_ok() {
            println!("{}", result);
        } else {
            eprintln!("{}", result);
        }
    }

    if let Err(e) = worker_thread.join() {
        panic::resume_unwind(e);
    }

    Ok(())
}

fn fingerprint_one_file<P: AsRef<path::Path>>(path: P) -> WorkResult {
    let mut file = match fs::File::open(&path) {
        Err(e) => return WorkResult::from_err(&path, e),
        Ok(f) => f,
    };
    let mut hasher = Sha256::new();

    if let Err(e) = io::copy(&mut file, &mut hasher) {
        return WorkResult::from_err(&path, e);
    }

    WorkResult::from_hash(&path, hasher.finalize())
}

impl WorkResult {
    fn from_err<P: AsRef<path::Path>>(path: P, err: io::Error) -> WorkResult {
        WorkResult {
            path: path.as_ref().to_path_buf(),
            result: Err(err),
        }
    }

    fn from_hash<P: AsRef<path::Path>, H: Into<Sha256Sum>>(path: P, hash: H) -> WorkResult {
        WorkResult {
            path: path.as_ref().to_path_buf(),
            result: Ok(hash.into()),
        }
    }
}

impl fmt::Display for WorkResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.result {
            Ok(hash) => write!(f, "{} : {}", hex::encode(&hash), self.path.display()),
            Err(err) => write!(f, "ERROR {} : {}", self.path.display(), err),
        }
    }
}
