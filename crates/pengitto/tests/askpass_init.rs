//! The credential prompt server starts once even when operations of
//! several repositories ask for it at the same moment. (Its own test binary,
//! so nothing else has started the server before.)

use std::{path::PathBuf, sync::Barrier};

use pengitto::askpass;

#[test]
fn concurrent_first_use_starts_one_server() {
    askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let barrier = std::sync::Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8u64)
        .map(|i| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let env = askpass::env_for(1000 + i, "Fetch", std::path::Path::new("/"), None).expect("prompts set up");
                PathBuf::from(env.iter().find(|(k, _)| k == askpass::SOCKET_ENV).unwrap().1.clone())
            })
        })
        .collect();
    let sockets: Vec<PathBuf> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(sockets.windows(2).all(|w| w[0] == w[1]), "{sockets:?}");
    // The one server is alive and answers (refusing an unknown token).
    assert_eq!(askpass::request(&sockets[0], "unknown", "", "Password: ").unwrap(), None);
    for i in 0..8 {
        askpass::forget(1000 + i);
    }
}
