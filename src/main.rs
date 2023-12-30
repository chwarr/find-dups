use digest::generic_array::GenericArray;
use sha2::{Digest, Sha256};
use std::convert::AsRef;
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::{fs, io};

type Sha256Sum = [u8; 32];

struct Fingerprint {
    pub path: PathBuf,
    pub hash: Sha256Sum,
}

fn main() -> io::Result<()> {
    for argument in env::args().skip(1) {
        let fingerprint = fingerprint_one_file(Path::new(&argument))?;
        println!("{}", fingerprint);
    }

    Ok(())
}

fn fingerprint_one_file<P: AsRef<Path>>(path: P) -> io::Result<Fingerprint> {
    let mut file = fs::File::open(&path)?;
    let mut hasher = Sha256::new();

    io::copy(&mut file, &mut hasher)?;

    let mut result = Fingerprint::new(&path);
    hasher.finalize_into(GenericArray::from_mut_slice(&mut result.hash));
    Ok(result)
}

impl Fingerprint {
    fn new<P: AsRef<Path>>(path: P) -> Fingerprint {
        Fingerprint {
            path: path.as_ref().to_path_buf(),
            hash: Sha256Sum::default(),
        }
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} : {}", hex::encode(&self.hash), self.path.display())
    }
}
