//! `open-ferry install-self`: what the install scripts run, from the binary
//! they downloaded and staged beside its destination, to put it in place.
//! Under the data directory's update lock it checks that no supervisor runs,
//! moves itself over the destination and writes the install receipt. Not for
//! people: the scripts run it.

use std::process::ExitCode;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "install-self";

/// Runs `install-self` with `args`, the arguments after its name.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let _ = (program, args.into_iter().count());
    unimplemented!("install-self comes with 2c-2")
}
