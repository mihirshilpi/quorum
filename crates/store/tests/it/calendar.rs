use quorum_model::{Calendar, CalendarId, EventId};
use quorum_store::{ErrorKind, Store};

use crate::common::{event, t};

fn cid(s: &str) -> CalendarId {
    CalendarId::new(s).unwrap()
}

async fn seeded() -> Store {
    let store = Store::open_in_memory().unwrap();
    let cals: Vec<Calendar> = ["work", "shared", "hidden"]
        .iter()
        .map(|id| serde_json::from_value(serde_json::json!({"id": id, "name": id})).unwrap())
        .collect();
    store
        .write(move |tx| {
            tx.upsert_calendars(&cals)?;
            let work = cid("work");
            let shared = cid("shared");
            let hidden = cid("hidden");
            // A 1-hour meeting at 09:00 Berlin (08:00 UTC in winter).
            tx.upsert_events(
                &work,
                &[event("e1", "work", "2026-01-15T09:00:00", "2026-01-15T10:00:00", "W. Europe Standard Time", false)],
            )?;
            // An all-day event in UTC on the 15th.
            tx.upsert_events(
                &shared,
                &[event("e2", "shared", "2026-01-15T00:00:00", "2026-01-16T00:00:00", "UTC", true)],
            )?;
            // A multi-day event starting days before the window.
            tx.upsert_events(
                &shared,
                &[event("e3", "shared", "2026-01-10T12:00:00", "2026-01-20T12:00:00", "UTC", false)],
            )?;
            // Outside the window.
            tx.upsert_events(
                &work,
                &[event("e4", "work", "2026-01-17T09:00:00", "2026-01-17T10:00:00", "UTC", false)],
            )?;
            // On a calendar not asked for.
            tx.upsert_events(
                &hidden,
                &[event("e5", "hidden", "2026-01-15T09:00:00", "2026-01-15T10:00:00", "UTC", false)],
            )?;
            // Unknown zone falls back to UTC; event without calendar id uses the given one.
            let mut e6 = event("e6", "work", "2026-01-15T20:00:00", "2026-01-15T21:00:00", "Mars/Olympus", false);
            e6.calendar_id = None;
            tx.upsert_events(&work, &[e6])?;
            Ok(())
        })
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn range_across_calendars_includes_all_day_and_multi_day() {
    let store = seeded().await;
    let page = store
        .events_in_range(vec![cid("work"), cid("shared")], t("2026-01-15T00:00:00Z"), t("2026-01-16T00:00:00Z"))
        .await
        .unwrap();
    let ids: Vec<_> = page.items.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["e3", "e2", "e1", "e6"], "ordered by UTC start");
    assert_eq!(page.items[3].calendar_id, Some(cid("work")));
}

#[tokio::test]
async fn range_boundaries_are_half_open() {
    let store = seeded().await;
    // Window ending exactly when e1 starts (08:00 UTC) excludes it; starting when it ends too.
    let before =
        store.events_in_range(vec![cid("work")], t("2026-01-15T07:00:00Z"), t("2026-01-15T08:00:00Z")).await.unwrap();
    assert!(before.items.iter().all(|e| e.id.as_str() != "e1"));
    let after =
        store.events_in_range(vec![cid("work")], t("2026-01-15T09:00:00Z"), t("2026-01-15T09:30:00Z")).await.unwrap();
    assert!(after.items.iter().all(|e| e.id.as_str() != "e1"));
    let inside =
        store.events_in_range(vec![cid("work")], t("2026-01-15T08:30:00Z"), t("2026-01-15T08:31:00Z")).await.unwrap();
    assert_eq!(inside.items.len(), 1);
}

#[tokio::test]
async fn delete_and_limits() {
    let store = seeded().await;
    store.apply_event_delta(cid("shared"), vec![], vec![EventId::new("e3").unwrap()]).await.unwrap();
    let page =
        store.events_in_range(vec![cid("shared")], t("2026-01-01T00:00:00Z"), t("2026-02-01T00:00:00Z")).await.unwrap();
    assert_eq!(page.items.len(), 1);
    let many: Vec<_> = (0..=quorum_store::MAX_RANGE_CALENDARS).map(|i| cid(&format!("c{i}"))).collect();
    let err = store.events_in_range(many, t("2026-01-01T00:00:00Z"), t("2026-02-01T00:00:00Z")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert!(
        store
            .events_in_range(vec![], t("2026-01-01T00:00:00Z"), t("2026-02-01T00:00:00Z"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(store.calendars().await.unwrap().items.len(), 3);
}
