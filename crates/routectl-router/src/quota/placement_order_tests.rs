//! The whole order the pace rule ranks candidates in, of which the best
//! bucket is the head.

use super::*;

#[test]
fn buckets_run_best_first_and_keep_candidate_order_inside_each() {
    // Arrange: two roomy seats in one rounded bucket, a lower roomy bucket, a
    // low_room seat and an unread one, offered out of index order.
    let standings = [
        unread(Reason::NoReading),
        scored(Tier::Roomy, 0.801),
        scored(Tier::LowRoom, 3.0),
        scored(Tier::Roomy, 0.5),
        scored(Tier::Roomy, 0.799),
    ];
    let candidates = [4, 0, 3, 2, 1];

    // Act
    let buckets = ordered_buckets(&standings, &candidates);

    // Assert
    assert_eq!(
        buckets,
        vec![
            (Tier::Roomy, vec![4, 1]),
            (Tier::Roomy, vec![3]),
            (Tier::LowRoom, vec![2]),
            (Tier::Unread, vec![0]),
        ]
    );
    assert_eq!(
        best_bucket(&standings, &candidates),
        buckets.into_iter().next()
    );
}
