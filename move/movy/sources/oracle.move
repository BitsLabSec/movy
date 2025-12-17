module movy::oracle;

use movy::log;
use std::string::String;

const ORACLE_FAILURE: u64 = 19260817;

public struct Crash has copy, drop { 
    reason: log::Log
}

public fun crash() {
    crash_with_details(log::make_log(vector[]))
}

public fun crash_because(reason: String) {
    crash_with_details(log::make_log(vector[
            log::make_keyed_entry(b"reason".to_string(), reason)
        ]))
}

public fun crash_with_details(details: log::Log) {
    sui::event::emit(Crash {
        reason: details
    });
}