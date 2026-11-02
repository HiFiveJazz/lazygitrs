/// Convert a Unix timestamp to local time, falling back to UTC if libc's
/// local-time conversion fails.
pub(super) fn local_from_unix(secs: i64) -> (i64, u32, u32, u32, u32) {
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(windows)]
        let ok = libc::localtime_s(&mut tm, &t) == 0;
        #[cfg(not(windows))]
        let ok = !libc::localtime_r(&t, &mut tm).is_null();
        if !ok {
            return civil_from_unix(secs);
        }
        (
            tm.tm_year as i64 + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
        )
    }
}

/// Howard Hinnant-style civil-from-days conversion used as the UTC fallback.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, minute)
}

/// Short English month name used by commit list/details timestamps.
pub(super) fn month_name(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}
