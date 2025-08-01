use std::io;

use rlimit::Resource;

/// Increase the file descriptor limit to the given minimum.
///
/// Panics if the hard limit is too low, otherwise tries to increase.
pub fn increase_nofile_limit(min_limit: u64) -> io::Result<u64> {
    let (soft, hard) = Resource::NOFILE.get()?;
    println!("At startup, file descriptor limit:      soft = {soft}, hard = {hard}");

    if hard < min_limit {
        panic!(
            "File descriptor hard limit is too low. Please increase it to at least {min_limit}."
        );
    }

    if soft != hard {
        Resource::NOFILE.set(hard, hard)?; // Just max things out to give us plenty of overhead.
        let (soft, hard) = Resource::NOFILE.get()?;
        println!("After increasing file descriptor limit: soft = {soft}, hard = {hard}");
    }

    Ok(soft)
}
