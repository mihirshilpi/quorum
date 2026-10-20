//! Layout math for the calendar grids, pure and tested natively:
//! - timed events become per-day segments (split at midnight) laid out side by side when they
//!   overlap ([`timed_segments`]);
//! - all-day and multi-day events, and every event in the month grid, are packed into lanes
//!   ([`lanes`]), with per-day overflow counts ([`overflow`]).

/// A timed event as the core places it: day indices (inclusive, may fall outside the view)
/// and minutes after midnight on the start and end day (`end_min` may be 1440).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timed {
    /// Start day index.
    pub start_day: i64,
    /// End day index (inclusive).
    pub end_day: i64,
    /// Start minute on the start day.
    pub start_min: u32,
    /// End minute on the end day.
    pub end_min: u32,
}

/// One piece of a timed event inside one day column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Index of the event in the input.
    pub idx: usize,
    /// Day column.
    pub day: usize,
    /// Top, minutes after midnight.
    pub top: u32,
    /// Bottom, minutes after midnight.
    pub bottom: u32,
    /// Column within its overlap group.
    pub col: usize,
    /// Columns in its overlap group.
    pub cols: usize,
    /// Whether the event starts in this column (else it continues from the day before).
    pub starts: bool,
}

/// Splits timed events into day segments and lays overlapping ones out side by side.
///
/// `min_len` is the shortest visual length in minutes (short events still need room for a
/// title), used both for drawing and for deciding what overlaps.
pub fn timed_segments(events: &[Timed], days: usize, min_len: u32) -> Vec<Segment> {
    let n = i64::try_from(days).unwrap_or(0);
    let mut per_day: Vec<Vec<Segment>> = vec![Vec::new(); days];
    for (idx, e) in events.iter().enumerate() {
        if e.end_day < e.start_day {
            continue;
        }
        let first = e.start_day.max(0);
        let last = e.end_day.min(n - 1);
        let mut d = first;
        while d <= last {
            let top = if d == e.start_day { e.start_min.min(1440) } else { 0 };
            let bottom = if d == e.end_day { e.end_min.min(1440) } else { 1440 };
            let bottom = bottom.max(top.saturating_add(min_len).min(1440)).max(top);
            if let Some(list) = usize::try_from(d).ok().and_then(|i| per_day.get_mut(i)) {
                list.push(Segment {
                    idx,
                    day: usize::try_from(d).unwrap_or(0),
                    top,
                    bottom,
                    col: 0,
                    cols: 1,
                    starts: d == e.start_day,
                });
            }
            d += 1;
        }
    }
    let mut out = Vec::new();
    for mut list in per_day {
        list.sort_by(|a, b| a.top.cmp(&b.top).then(b.bottom.cmp(&a.bottom)).then(a.idx.cmp(&b.idx)));
        // Clusters of transitively overlapping segments share a column count.
        let mut cluster: Vec<Segment> = Vec::new();
        let mut cluster_end = 0u32;
        for s in list {
            if !cluster.is_empty() && s.top >= cluster_end {
                out.extend(place_cluster(std::mem::take(&mut cluster)));
            }
            cluster_end = if cluster.is_empty() { s.bottom } else { cluster_end.max(s.bottom) };
            cluster.push(s);
        }
        out.extend(place_cluster(cluster));
    }
    out
}

fn place_cluster(mut cluster: Vec<Segment>) -> Vec<Segment> {
    let mut col_ends: Vec<u32> = Vec::new();
    for s in &mut cluster {
        let col = col_ends.iter().position(|end| *end <= s.top).unwrap_or(col_ends.len());
        if let Some(end) = col_ends.get_mut(col) {
            *end = s.bottom;
        } else {
            col_ends.push(s.bottom);
        }
        s.col = col;
    }
    let cols = col_ends.len().max(1);
    for s in &mut cluster {
        s.cols = cols;
    }
    cluster
}

/// A span packed into a lane, clipped to the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lane {
    /// Index of the item in the input.
    pub idx: usize,
    /// Lane (row) number, 0 at the top.
    pub lane: usize,
    /// First visible day.
    pub first: usize,
    /// Last visible day (inclusive).
    pub last: usize,
    /// Starts before the view.
    pub cont_left: bool,
    /// Ends after the view.
    pub cont_right: bool,
}

/// Packs day spans (`(start_day, end_day)`, inclusive, may exceed the view) into the fewest
/// lanes, in input order (sort the input by priority first). Returns lanes and their count.
pub fn lanes(spans: &[(i64, i64)], days: usize) -> (Vec<Lane>, usize) {
    let n = i64::try_from(days).unwrap_or(0);
    let mut rows: Vec<Vec<bool>> = Vec::new();
    let mut out = Vec::new();
    for (idx, &(s, e)) in spans.iter().enumerate() {
        if e < s || e < 0 || s >= n {
            continue;
        }
        let first = usize::try_from(s.max(0)).unwrap_or(0);
        let last = usize::try_from(e.min(n - 1)).unwrap_or(0);
        let lane = rows
            .iter()
            .position(|row| row.get(first..=last).is_some_and(|cells| cells.iter().all(|c| !c)))
            .unwrap_or(rows.len());
        if lane == rows.len() {
            rows.push(vec![false; days]);
        }
        if let Some(cells) = rows.get_mut(lane).and_then(|r| r.get_mut(first..=last)) {
            cells.iter_mut().for_each(|c| *c = true);
        }
        out.push(Lane { idx, lane, first, last, cont_left: s < 0, cont_right: e >= n });
    }
    (out, rows.len())
}

/// For each day, how many lanes at or beyond `max_lanes` cover it (the "+N more" count).
pub fn overflow(lanes: &[Lane], days: usize, max_lanes: usize) -> Vec<usize> {
    let mut out = vec![0usize; days];
    for l in lanes.iter().filter(|l| l.lane >= max_lanes) {
        for c in out.iter_mut().take(l.last.saturating_add(1)).skip(l.first) {
            *c = c.saturating_add(1);
        }
    }
    out
}

/// Pixel offset of `minutes` in a grid of `hour_px` per hour.
pub fn minutes_to_px(minutes: u32, hour_px: f64) -> f64 {
    f64::from(minutes) * hour_px / 60.0
}

/// The minute under a pixel offset, snapped to `snap` minutes (clamped to the day).
pub fn px_to_minutes(px: f64, hour_px: f64, snap: u32) -> u32 {
    if !px.is_finite() || !hour_px.is_finite() || hour_px <= 0.0 || snap == 0 {
        return 0;
    }
    let raw = (px / hour_px * 60.0).clamp(0.0, 1439.0);
    // Truncation is the point: we want the slot the pointer is in.
    let m = raw as u32;
    (m / snap) * snap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sd: i64, sm: u32, ed: i64, em: u32) -> Timed {
        Timed { start_day: sd, end_day: ed, start_min: sm, end_min: em }
    }

    #[test]
    fn overlapping_events_sit_side_by_side() {
        let ev = [t(0, 540, 0, 630), t(0, 570, 0, 600), t(0, 600, 0, 660), t(0, 720, 0, 750)];
        let s = timed_segments(&ev, 1, 15);
        let by = |i: usize| s.iter().find(|x| x.idx == i).copied().unwrap();
        assert_eq!((by(0).col, by(0).cols), (0, 2));
        assert_eq!((by(1).col, by(1).cols), (1, 2));
        assert_eq!((by(2).col, by(2).cols), (1, 2), "reuses the freed column");
        assert_eq!((by(3).col, by(3).cols), (0, 1), "separate cluster");
    }

    #[test]
    fn events_split_at_midnight_and_clip_to_the_view() {
        let s = timed_segments(&[t(0, 1320, 1, 60), t(-1, 1200, 0, 30), t(2, 0, 9, 0)], 3, 15);
        let first: Vec<_> = s.iter().filter(|x| x.idx == 0).map(|x| (x.day, x.top, x.bottom, x.starts)).collect();
        assert_eq!(first, [(0, 1320, 1440, true), (1, 0, 60, false)]);
        let second: Vec<_> = s.iter().filter(|x| x.idx == 1).map(|x| (x.day, x.top, x.bottom)).collect();
        assert_eq!(second, [(0, 0, 30)]);
        assert_eq!(s.iter().filter(|x| x.idx == 2).count(), 1);
        // Zero-length events still get a visible height; nonsense is skipped.
        let s = timed_segments(&[t(0, 600, 0, 600), t(1, 0, 0, 0)], 1, 15);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].bottom, 615);
        assert!(timed_segments(&[t(0, 9999, 0, 99_999)], 1, 15).iter().all(|x| x.bottom <= 1440));
    }

    #[test]
    fn all_day_lanes_pack_and_report_overflow() {
        let spans = [(0, 2), (1, 1), (3, 6), (-2, 0), (2, 9), (1, 3)];
        let (l, count) = lanes(&spans, 7);
        let lane = |i: usize| l.iter().find(|x| x.idx == i).map(|x| x.lane).unwrap();
        assert_eq!(lane(0), 0);
        assert_eq!(lane(1), 1);
        assert_eq!(lane(2), 0);
        assert_eq!(lane(3), 1, "day 0 is free in lane 1");
        let cont = l.iter().find(|x| x.idx == 3).unwrap();
        assert!(cont.cont_left && cont.first == 0);
        let right = l.iter().find(|x| x.idx == 4).unwrap();
        assert!(right.cont_right && right.last == 6);
        assert!(count >= 3);
        let o = overflow(&l, 7, 2);
        assert_eq!(o.len(), 7);
        assert!(o.iter().sum::<usize>() > 0);
        assert!(lanes(&[(5, 1), (10, 12), (-5, -1)], 7).0.is_empty());
    }

    #[test]
    fn pixel_math_is_total() {
        assert_eq!(minutes_to_px(90, 48.0), 72.0);
        assert_eq!(px_to_minutes(72.0, 48.0, 15), 90);
        assert_eq!(px_to_minutes(-5.0, 48.0, 15), 0);
        assert_eq!(px_to_minutes(f64::NAN, 48.0, 15), 0);
        assert_eq!(px_to_minutes(1e9, 48.0, 30), 1410);
        assert_eq!(px_to_minutes(10.0, 0.0, 15), 0);
    }
}
