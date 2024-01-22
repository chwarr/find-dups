use clap::Parser;
use crossbeam::channel::{unbounded, Receiver, Sender};
use sha2::{Digest, Sha256};
use std::convert::AsRef;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::panic;
use std::path;
use std::thread;
use std::thread::JoinHandle;
use std::vec::Vec;

type Sha256Sum = [u8; 32];

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Paths that make up the "left-hand" side of the comparison. Can be
    /// repeated.
    #[arg(long, required = true, short = 'l')]
    left: Vec<OsString>,
    /// Paths that make up the "right-hand" side of the comparison. Can be
    /// repeated.
    #[arg(long, required = true, short = 'r')]
    right: Vec<OsString>,
}

enum Work {
    Directory {
        path: PathLocation,
        work_sender: Sender<Work>,
    },
    File {
        path: PathLocation,
    },
}

struct WorkResult {
    pub path: PathLocation,
    pub result: io::Result<Sha256Sum>,
}

#[derive(Clone)]
enum PathLocation {
    Left(path::PathBuf),
    Right(path::PathBuf),
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let (work_sender, work_receiver) = unbounded();
    let (results_sender, results_receiver) = unbounded();

    enqueue_initial_work_from_args(&args, &work_sender);

    // Initial work has been enqueued. Any Directory work has its own clone
    // of work_sender that is can use to enqueue more work.
    //
    // Drop this copy of the sender so that all the copies are dropped when
    // directory enumeration is complete.
    drop(work_sender);

    let worker_threads = start_worker_threads(work_receiver, results_sender);

    for result in results_receiver.iter() {
        if result.result.is_ok() {
            println!("{}", result);
        } else {
            eprintln!("{}", result);
        }
    }

    for worker_thread in worker_threads {
        if let Err(e) = worker_thread.join() {
            panic::resume_unwind(e);
        }
    }

    Ok(())
}

fn enqueue_initial_work_from_args(args: &Args, work_sender: &Sender<Work>) {
    enqueue_initial_work_for_side(
        args.left.iter(),
        |path: &path::Path| -> PathLocation { PathLocation::new_left(path) },
        work_sender,
    );
    enqueue_initial_work_for_side(
        args.right.iter(),
        |path: &path::Path| -> PathLocation { PathLocation::new_right(path) },
        work_sender,
    );
}

fn enqueue_initial_work_for_side<'a, I, F>(
    arg_paths: I,
    path_location_factory: F,
    work_sender: &Sender<Work>,
) where
    I: IntoIterator<Item = &'a OsString>,
    F: Fn(&path::Path) -> PathLocation,
{
    for arg_path in arg_paths.into_iter() {
        let path = path::Path::new(&arg_path);

        if path.is_symlink() {
            eprintln!(
                "WARN: Symlinks are not supported: '{}'",
                &arg_path.to_string_lossy()
            );
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
                path: path_location_factory(path),
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
                path: path_location_factory(path),
            };
            work_sender
                .send(work)
                .expect("Unable to enqueue initial File work into work channel");
        }
    }
}

fn start_worker_threads(
    work_receiver: Receiver<Work>,
    results_sender: Sender<WorkResult>,
) -> Vec<JoinHandle<()>> {
    let num_threads: usize = thread::available_parallelism()
        .unwrap_or(NonZeroUsize::new(2).unwrap())
        .into();

    let mut results = Vec::with_capacity(num_threads);

    for _ in 0..num_threads {
        let thread_work_receiver = work_receiver.clone();
        let thread_results_sender = results_sender.clone();

        results.push(thread::spawn(move || {
            for work in thread_work_receiver.iter() {
                match work {
                    Work::Directory { path, work_sender } => {
                        handle_dir_work(path, &work_sender, &thread_results_sender)
                    }
                    Work::File { path } => handle_file_work(path, &thread_results_sender),
                };
            }
        }));
    }

    results
}

fn handle_dir_work(
    path: PathLocation,
    work_sender: &Sender<Work>,
    results_sender: &Sender<WorkResult>,
) {
    let read_dir = match fs::read_dir(path.path()) {
        Err(e) => {
            let r = WorkResult::from_err(path, e);
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
                let r = WorkResult::from_err(path.clone(), e);
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
                PathLocation::new_same_side(&path, &entry_path),
                io::Error::other("Symlinks are not supported. Ignoring."),
            );

            results_sender
                .send(r)
                .expect("Unable to enqueue result into result channel");

            continue;
        } else if entry_path.is_dir() {
            let w = Work::Directory {
                path: PathLocation::new_same_side(&path, &entry_path),
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
                path: PathLocation::new_same_side(&path, &entry_path),
            };
            work_sender
                .send(w)
                .expect("Unable to enqueue File into work channel");
        }
    }
}

fn handle_file_work(path: PathLocation, results_sender: &Sender<WorkResult>) {
    let r = fingerprint_one_file(path);

    results_sender
        .send(r)
        .expect("Unable to enqueue result into result channel");
}

fn fingerprint_one_file(path: PathLocation) -> WorkResult {
    let mut file = match fs::File::open(path.path()) {
        Err(e) => return WorkResult::from_err(path, e),
        Ok(f) => f,
    };

    let mut hasher = Sha256::new();

    match io::copy(&mut file, &mut hasher) {
        Err(e) => WorkResult::from_err(path, e),
        Ok(_) => WorkResult::from_hash(path, hasher.finalize()),
    }
}

impl WorkResult {
    fn from_err(path: PathLocation, err: io::Error) -> WorkResult {
        WorkResult {
            path,
            result: Err(err),
        }
    }

    fn from_hash<H: Into<Sha256Sum>>(path: PathLocation, hash: H) -> WorkResult {
        WorkResult {
            path,
            result: Ok(hash.into()),
        }
    }
}

impl fmt::Display for WorkResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.result {
            Ok(hash) => write!(f, "OK: {} : {}", self.path, hex::encode(hash)),
            Err(err) => write!(f, "ERROR: {} : {}", self.path, err),
        }
    }
}

impl PathLocation {
    fn new_left<P: AsRef<path::Path>>(path: P) -> PathLocation {
        PathLocation::Left(path.as_ref().to_path_buf())
    }

    fn new_right<P: AsRef<path::Path>>(path: P) -> PathLocation {
        PathLocation::Right(path.as_ref().to_path_buf())
    }

    fn new_same_side<P: AsRef<path::Path>>(other: &PathLocation, path: P) -> PathLocation {
        match other {
            PathLocation::Left(_) => PathLocation::Left(path.as_ref().to_path_buf()),
            PathLocation::Right(_) => PathLocation::Right(path.as_ref().to_path_buf()),
        }
    }

    fn path(&self) -> &path::Path {
        match self {
            PathLocation::Left(path) => path,
            PathLocation::Right(path) => path,
        }
    }
}

impl fmt::Display for PathLocation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PathLocation::Left(path) => write!(f, "<= '{}'", path.display()),
            PathLocation::Right(path) => write!(f, "=> '{}'", path.display()),
        }
    }
}
