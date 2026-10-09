//! Not upstream's: a never-run link probe for the release seam-string check.
//! Build without `blue-green-test`; keep the disk APIs linked without calling them.

use open_ferry_update::journal::JournalStore;
use open_ferry_update::state::State;

fn main() {
    std::hint::black_box(
        JournalStore::write
            as fn(
                &JournalStore,
                &mut open_ferry_update::journal::Journal,
                &str,
            ) -> Result<(), open_ferry_update::journal::JournalError>,
    );
    std::hint::black_box(
        State::save_named as fn(&State, &std::path::Path, &str) -> std::io::Result<()>,
    );
    std::hint::black_box(
        open_ferry_update::test_seams::hit as fn(&std::path::Path, &str) -> std::io::Result<()>,
    );
    // Not upstream's: prune reaches staging cleanup synchronously; retain
    // post-commit writers and async staging entry-point addresses too,
    // without ever calling any of them.
    std::hint::black_box(JournalStore::quarantine as *const ());
    std::hint::black_box(State::save_committed_named as *const ());
    std::hint::black_box(open_ferry_update::Updater::prune as *const ());
    std::hint::black_box(open_ferry_update::Updater::stage as *const ());
    std::hint::black_box(open_ferry_update::staging::stage_exclusive as *const ());
}
