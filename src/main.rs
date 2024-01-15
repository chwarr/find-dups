use crossbeam::channel::{unbounded, Receiver, Sender};
use sha2::{Digest, Sha256};
use std::convert::AsRef;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::panic;
use std::path;
use std::thread;
use std::thread::JoinHandle;

type Sha256Sum = [u8; 32];

enum Work {
    Directory {
        path: path::PathBuf,
        work_sender: Sender<Work>,
    },
    File {
        path: path::PathBuf,
    },
}

struct WorkResult {
    pub path: path::PathBuf,
    pub result: io::Result<Sha256Sum>,
}

fn main() -> io::Result<()> {
    let (work_sender, work_receiver) = unbounded();
    let (results_sender, results_receiver) = unbounded();

    enqueue_initial_work_from_args(&work_sender);

    // Initial work has been enqueued. Any Directory work has its own clone
    // of work_sender that is can use to enqueue more work.
    //
    // Drop this copy of the sender so that all the copies are dropped when
    // directory enumeration is complete.
    drop(work_sender);

    let worker_thread = start_worker_thread(work_receiver, results_sender);

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

fn enqueue_initial_work_from_args(work_sender: &Sender<Work>) {
    for argument in env::args().skip(1) {
        let path = path::Path::new(&argument);

        if path.is_symlink() {
            eprintln!("WARN: Symlinks are not supported: '{}'", &argument);
            continue;
        }

        let metadata = match path.metadata() {
            Err(e) => {
                eprintln!(
                    "WARN: unable to process path on command line: '{}': {}",
                    path.display(),
                    e
                );
                continue;
            }
            Ok(metadata) => metadata,
        };

        if metadata.is_dir() {
            let work = Work::Directory {
                path: path::PathBuf::from(&path),
                work_sender: work_sender.clone(),
            };
            work_sender
                .send(work)
                .expect("Unable to enqueue initial Directory work into work channel");
        } else {
            assert!(
                metadata.is_file(),
                "Exptected path '{}' to be a file on this path, but it wasn't.",
                path.display()
            );

            let work = Work::File {
                path: path::PathBuf::from(&path),
            };
            work_sender
                .send(work)
                .expect("Unable to enqueue initial File work into work channel");
        }
    }
}

fn start_worker_thread(
    work_receiver: Receiver<Work>,
    results_sender: Sender<WorkResult>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        for work in work_receiver.iter() {
            match work {
                Work::Directory { path, work_sender } => {
                    handle_dir_work(&path, &work_sender, &results_sender)
                }
                Work::File { path } => handle_file_work(&path, &results_sender),
            };
        }
    })
}

fn handle_dir_work<P: AsRef<path::Path>>(
    path: P,
    work_sender: &Sender<Work>,
    results_sender: &Sender<WorkResult>,
) {
    let read_dir = match fs::read_dir(&path) {
        Err(e) => {
            let r = WorkResult::from_err(&path, e);
            results_sender
                .send(r)
                .expect("Unable to enqueue result into result channel");
            return;
        }
        Ok(read_dir) => read_dir,
    };

    for entry in read_dir {
        let entry = match entry {
            Err(e) => {
                let r = WorkResult::from_err(&path, e);
                results_sender
                    .send(r)
                    .expect("Unable to enqueue result into result channel");
                continue;
            }
            Ok(entry) => entry,
        };

        let entry_path = entry.path();
        if entry_path.is_symlink() {
            let r = WorkResult::from_err(
                &path,
                io::Error::other("Symlinks are not supported. Ignoring."),
            );

            results_sender
                .send(r)
                .expect("Unable to enqueue result into result channel");

            continue;
        } else if entry_path.is_dir() {
            let w = Work::Directory {
                path: entry_path.into(),
                work_sender: work_sender.clone(),
            };
            work_sender
                .send(w)
                .expect("Unable to enqueue Directory into work channel");
        } else {
            assert!(
                entry_path.is_file(),
                "Expected path '{}' to be a file on this path, but it wasn't.",
                entry_path.display()
            );
            let w = Work::File {
                path: entry_path.into(),
            };
            work_sender
                .send(w)
                .expect("Unable to enqueue File into work channel");
        }
    }
}

fn handle_file_work<P: AsRef<path::Path>>(path: P, results_sender: &Sender<WorkResult>) {
    let r = fingerprint_one_file(path);

    results_sender
        .send(r)
        .expect("Unable to enqueue result into result channel");
}

fn fingerprint_one_file<P: AsRef<path::Path>>(path: P) -> WorkResult {
    let mut file = match fs::File::open(&path) {
        Err(e) => return WorkResult::from_err(&path, e),
        Ok(f) => f,
    };

    let mut hasher = Sha256::new();

    match io::copy(&mut file, &mut hasher) {
        Err(e) => WorkResult::from_err(&path, e),
        Ok(_) => WorkResult::from_hash(&path, hasher.finalize()),
    }
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
            Ok(hash) => write!(f, "OK: '{}' : {}", self.path.display(), hex::encode(&hash)),
            Err(err) => write!(f, "ERROR: '{}' : {}", self.path.display(), err),
        }
    }
}
