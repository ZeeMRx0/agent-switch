//! Tiny keychain helper used by the agent-switch CLI and menu bar app.
//!
//! `agent-switch-keychain get|set|delete <service> <account>`; `set` reads the value from stdin until EOF
//! (never argv), `get` prints it to stdout. Exit codes: 0 ok, 44 not found, 1 error (message on stderr
//! never contains a value). Signed with a stable identity by `scripts/sign.sh`, so items it creates keep
//! trusting it across rebuilds. Uses SecItem* via security-framework, so there is no 4 KB command limit.

use std::io::{Read, Write};
use std::process::ExitCode;

use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const NOT_FOUND_EXIT: u8 = 44;
const MAX_VALUE: u64 = 1 << 20;

fn fail(msg: &str) -> ExitCode {
    eprintln!("agent-switch-keychain: {msg}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, verb, service, account] = args.as_slice() else {
        return fail("usage: agent-switch-keychain get|set|delete <service> <account>");
    };

    match verb.as_str() {
        "get" => match get_generic_password(service, account) {
            Ok(bytes) => match std::io::stdout().write_all(&bytes) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&format!("write stdout: {e}")),
            },
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => ExitCode::from(NOT_FOUND_EXIT),
            Err(e) => fail(&format!("get failed (OSStatus {})", e.code())),
        },
        "set" => {
            let mut value = Vec::new();
            if let Err(e) = std::io::stdin().take(MAX_VALUE + 1).read_to_end(&mut value) {
                return fail(&format!("read stdin: {e}"));
            }
            if value.is_empty() || value.len() as u64 > MAX_VALUE {
                return fail("value must be 1 byte to 1 MiB");
            }
            match set_generic_password(service, account, &value) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&format!("set failed (OSStatus {})", e.code())),
            }
        }
        "delete" => match delete_generic_password(service, account) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => ExitCode::from(NOT_FOUND_EXIT),
            Err(e) => fail(&format!("delete failed (OSStatus {})", e.code())),
        },
        other => fail(&format!("unknown verb `{other}`")),
    }
}
