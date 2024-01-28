use clap::Parser;
use crossbeam::channel::{unbounded, Receiver, Sender};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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

    /// Omit printing files that only exist on the left-hand side. Defaults
    /// to printing them.
    #[arg(long, short = 'L')]
    omit_left: bool,

    /// Omit printing files that only exist on the right-hand side. Defaults
    /// to printing them.
    #[arg(long, short = 'R')]
    omit_right: bool,

    /// Print the files present in both the left- and right-hand sides.
    /// Defaults to omitting them.
    #[arg(long, short = 'B')]
    show_both: bool,
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

    let mut left: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();
    let mut right: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();

    for work_result in results_receiver.iter() {
        if work_result.result.is_err() {
            eprintln!("{}", work_result);
            continue;
        }

        let sha256sum = work_result.result.unwrap();

        match work_result.path {
            PathLocation::Left(path) => add_to_result_hash_map(&mut left, sha256sum, path),
            PathLocation::Right(path) => add_to_result_hash_map(&mut right, sha256sum, path),
        }
    }

    for worker_thread in worker_threads {
        if let Err(e) = worker_thread.join() {
            panic::resume_unwind(e);
        }
    }

    let mut locations = split_into_locations(left, right);

    if !args.omit_left {
        locations.left.sort_unstable();
        for path in locations.left {
            println!("<= '{}'", path.display());
        }
    }

    if !args.omit_right {
        locations.right.sort_unstable();
        for path in locations.right {
            println!("=> '{}'", path.display());
        }
    }

    if args.show_both {
        for (lpaths, rpaths) in locations.both.iter_mut() {
            lpaths.sort_unstable();
            rpaths.sort_unstable();
        }

        // Sort 'both' locations by their first lpath. The vectors are
        // guaranteed to be non-empty, otherwise this wouldn't be a 'both'
        // location.
        locations
            .both
            .sort_unstable_by(|(lpaths_l, _), (lpaths_r, _)| {
                std::cmp::Ord::cmp(&lpaths_l[0], &lpaths_r[0])
            });

        for (lpaths, rpaths) in locations.both {
            println!("<=>");
            for lpath in lpaths {
                println!("  <= '{}'", lpath.display());
            }
            for rpath in rpaths {
                println!("  => '{}'", rpath.display());
            }
        }
    }

    Ok(())
}

fn add_to_result_hash_map(
    map: &mut HashMap<Sha256Sum, Vec<path::PathBuf>>,
    hash: Sha256Sum,
    path: path::PathBuf,
) {
    map.entry(hash)
        .or_insert_with(|| Vec::with_capacity(1))
        .push(path);
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

struct Locations {
    left: Vec<path::PathBuf>,
    both: Vec<(Vec<path::PathBuf>, Vec<path::PathBuf>)>,
    right: Vec<path::PathBuf>,
}

fn split_into_locations(
    mut left: HashMap<Sha256Sum, Vec<path::PathBuf>>,
    mut right: HashMap<Sha256Sum, Vec<path::PathBuf>>,
) -> Locations {
    // When extract_if is stabalized, I think this can be replaced by that.
    // https://github.com/rust-lang/rust/issues/59618
    let keys_in_both: HashSet<Sha256Sum> = left
        .keys()
        .filter_map(|k| {
            if right.contains_key(k) {
                Some(*k)
            } else {
                None
            }
        })
        .collect();

    let both_results: Vec<(Vec<path::PathBuf>, Vec<path::PathBuf>)> = keys_in_both
        .iter()
        .map(|k| {
            // The key was present in both, so unwrapping the Option from
            // .remove shouldn't panic.
            let from_left = left.remove(k).unwrap();
            let from_right = right.remove(k).unwrap();
            (from_left, from_right)
        })
        .collect();

    // The items present in both have already been removed, so consuming the
    // values to create the results should yield only left/right paths.
    let left_results: Vec<path::PathBuf> = left.into_values().flatten().collect();
    let right_results: Vec<path::PathBuf> = right.into_values().flatten().collect();

    Locations {
        left: left_results,
        both: both_results,
        right: right_results,
    }
}

#[test]
fn split_nothing_right_only_left() {
    let some_sha256_sum1: Sha256Sum = [1u8; 32];
    let some_sha256_sum2: Sha256Sum = [2u8; 32];

    let mut left: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();
    left.insert(some_sha256_sum1, vec!["lpath1".into()]);
    left.insert(some_sha256_sum2, vec!["lpath2".into()]);

    let right: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();

    let results: Locations = split_into_locations(left, right);

    assert_eq!(results.left.len(), 2);
    assert!(results.both.is_empty());
    assert!(results.right.is_empty());
}

#[test]
fn split_nothing_left_only_right() {
    let some_sha256_sum1: Sha256Sum = [1u8; 32];
    let some_sha256_sum2: Sha256Sum = [2u8; 32];

    let left: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();

    let mut right: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();
    right.insert(some_sha256_sum1, vec!["rpath1".into()]);
    right.insert(some_sha256_sum2, vec!["rpath2".into()]);

    let results: Locations = split_into_locations(left, right);

    assert!(results.left.is_empty());
    assert!(results.both.is_empty());
    assert_eq!(results.right.len(), 2);
}

#[test]
fn split_mix_has_expected_values() {
    let some_sha256_sum_l: Sha256Sum = [1u8; 32];
    let some_sha256_sum_r: Sha256Sum = [2u8; 32];
    let some_sha256_sum_b: Sha256Sum = [4u8; 32];

    let mut left: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();
    left.insert(
        some_sha256_sum_l,
        vec!["lpath1_a".into(), "lpath2_a".into()],
    );
    left.insert(some_sha256_sum_b.clone(), vec!["bpath1_l".into()]);

    let mut right: HashMap<Sha256Sum, Vec<path::PathBuf>> = HashMap::new();
    right.insert(some_sha256_sum_r, vec!["rpath1".into()]);
    right.insert(
        some_sha256_sum_b.clone(),
        vec!["bpath1_r".into(), "bpath2_r".into()],
    );

    let mut results: Locations = split_into_locations(left, right);

    results.left.sort_unstable();
    assert_eq!(
        results.left,
        vec![
            path::PathBuf::from("lpath1_a"),
            path::PathBuf::from("lpath2_a")
        ]
    );
    assert_eq!(
        results.both,
        vec![(
            vec![path::PathBuf::from("bpath1_l")],
            vec![
                path::PathBuf::from("bpath1_r"),
                path::PathBuf::from("bpath2_r")
            ]
        )]
    );
    assert_eq!(results.right, vec![path::PathBuf::from("rpath1")]);
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
