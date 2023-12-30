use digest::generic_array::GenericArray;
use sha2::{Digest, Sha256};
use std::convert::AsRef;
use std::env;
use std::path::Path;
use std::{fs, io};

type Sha256Sum = [u8; 32];

fn main() -> io::Result<()> {
    for argument in env::args().skip(1) {
        let hash = hash_one_file(Path::new(&argument))?;
        println!("{} : {}", hex::encode(&hash), &argument);
    }

    Ok(())
}

fn hash_one_file<P: AsRef<Path>>(path: P) -> io::Result<Sha256Sum> {
    let mut file = fs::File::open(&path)?;
    let mut hasher = Sha256::new();

    io::copy(&mut file, &mut hasher)?;

    let mut hash = Sha256Sum::default();
    hasher.finalize_into(GenericArray::from_mut_slice(&mut hash));
    Ok(hash)
}
