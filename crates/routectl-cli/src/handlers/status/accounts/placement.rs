//! The `/status/accounts` `placement` object: the pace rule's id, its
//! counters since the daemon started and, per pool and upstream model, each
//! seat's rank, tier, score and reason. Top-level rather than per account
//! row because one account can rank differently in each pool and model.

use serde::Serialize;

use routectl_router::{
    PLACEMENT_RULE, PlacementBirths, PlacementMoves, PlacementStatus, PoolPlacement, SeatPlacement,
};

#[derive(Debug, Clone, Serialize)]
pub(in crate::handlers::status) struct PlacementRow {
    /// The placement rule's id, as the usage ledger's record names it.
    rule: &'static str,
    /// Every count below runs from the daemon's start.
    since_start: bool,
    births: BirthsRow,
    moves: MovesRow,
    /// `breaker_open` moves whose old seat stood `low_room` at the move.
    cap_forced_moves: u64,
    pools: Vec<PoolRow>,
}

/// Won births, keyless placements included, by the placed seat's tier.
#[derive(Debug, Clone, Copy, Serialize)]
struct BirthsRow {
    roomy: u64,
    low_room: u64,
    unread: u64,
}

/// Won moves by the refusal the conversation left its seat for.
#[derive(Debug, Clone, Copy, Serialize)]
struct MovesRow {
    credential_unusable: u64,
    account_parked: u64,
    breaker_open: u64,
}

#[derive(Debug, Clone, Serialize)]
struct PoolRow {
    pool: String,
    /// The upstream model id the ranking is for.
    model: String,
    /// One entry per seat, in the pool's member order.
    seats: Vec<SeatRow>,
}

#[derive(Debug, Clone, Serialize)]
struct SeatRow {
    /// The key `accounts[].account` carries; null only for a member that
    /// forwards the client's credential.
    account: Option<String>,
    /// Dense from 1, shared by tied seats; null while the seat is
    /// unavailable.
    rank: Option<usize>,
    /// `roomy`, `low_room` or `unread`.
    tier: &'static str,
    /// Two decimals; null when the seat has no score.
    score: Option<f64>,
    /// A closed token: a standing reason, or an eligibility reason while
    /// the seat is unavailable.
    reason: &'static str,
}

pub(in crate::handlers::status) fn map_placement(status: PlacementStatus) -> PlacementRow {
    PlacementRow {
        rule: PLACEMENT_RULE,
        since_start: true,
        births: map_births(status.births),
        moves: map_moves(status.moves),
        cap_forced_moves: status.cap_forced_moves,
        pools: status.pools.into_iter().map(map_pool).collect(),
    }
}

const fn map_births(births: PlacementBirths) -> BirthsRow {
    BirthsRow {
        roomy: births.roomy,
        low_room: births.low_room,
        unread: births.unread,
    }
}

const fn map_moves(moves: PlacementMoves) -> MovesRow {
    MovesRow {
        credential_unusable: moves.credential_unusable,
        account_parked: moves.account_parked,
        breaker_open: moves.breaker_open,
    }
}

fn map_pool(pool: PoolPlacement) -> PoolRow {
    PoolRow {
        pool: pool.pool,
        model: pool.model,
        seats: pool.seats.into_iter().map(map_seat).collect(),
    }
}

fn map_seat(seat: SeatPlacement) -> SeatRow {
    SeatRow {
        account: seat.account,
        rank: seat.rank,
        tier: seat.tier,
        score: seat.score,
        reason: seat.reason,
    }
}
