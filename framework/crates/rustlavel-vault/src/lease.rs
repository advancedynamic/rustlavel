//! Leases: secrets that stop working if nobody renews them.
//!
//! Everything dynamic in Vault comes with a lease — a database account, a
//! cloud credential, the token you authenticated with. The lease is the point:
//! a credential that expires on its own is one that a leaked backup or an old
//! log file cannot be used with tomorrow.
//!
//! It also means a long-running process has a job it did not have before. Miss
//! the renewal and the credential stops working *in production, under load*,
//! which is the worst possible time to discover you were supposed to renew it.

use std::time::{Duration, Instant};

/// A lease on something the store issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The handle used to renew or revoke. Empty for something with no lease,
    /// such as a value read from a static KV path.
    pub id: String,
    pub duration: Duration,
    pub renewable: bool,
    /// When this lease was issued, as this process measures time.
    ///
    /// `Instant` rather than a wall clock on purpose: renewal is about elapsed
    /// time, and a wall clock can be stepped backwards by NTP, which would make
    /// a credential look fresh long after it stopped working.
    pub issued: Instant,
}

impl Lease {
    /// A value that carries no lease and never expires — a static KV secret.
    pub fn none() -> Lease {
        Lease {
            id: String::new(),
            duration: Duration::ZERO,
            renewable: false,
            issued: Instant::now(),
        }
    }

    pub fn new(id: impl Into<String>, duration: Duration, renewable: bool) -> Lease {
        Lease { id: id.into(), duration, renewable, issued: Instant::now() }
    }

    /// Whether there is a lease at all.
    pub fn exists(&self) -> bool {
        !self.id.is_empty() || !self.duration.is_zero()
    }

    pub fn elapsed(&self) -> Duration {
        self.issued.elapsed()
    }

    /// How long is left, saturating at zero.
    pub fn remaining(&self) -> Duration {
        self.duration.saturating_sub(self.elapsed())
    }

    pub fn is_expired(&self) -> bool {
        self.exists() && self.remaining().is_zero()
    }

    /// When to renew: at two thirds of the lease.
    ///
    /// Vault's own agent uses the same fraction, and the reasoning is worth
    /// stating — it leaves a full third of the lease as room for a renewal that
    /// fails, so a transient outage costs a retry rather than an expired
    /// credential. Renewing at 95% would technically work and would leave no
    /// room to fail at all.
    pub fn renew_after(&self) -> Duration {
        if !self.exists() || !self.renewable {
            return Duration::MAX;
        }

        // A very short lease gets renewed immediately rather than never: the
        // two-thirds point of four seconds has probably already passed by the
        // time anybody looks.
        let target = self.duration.mul_f64(2.0 / 3.0);
        target.saturating_sub(self.elapsed())
    }

    /// Whether it is time to renew.
    pub fn should_renew(&self) -> bool {
        self.renewable && self.exists() && self.renew_after().is_zero()
    }

    /// Replace the timing after a successful renewal, keeping the id.
    ///
    /// The id survives because renewing does not issue a new lease; it extends
    /// the one that exists. Something that *does* issue a new one — a rotation
    /// — replaces the whole `Lease`.
    pub fn renewed(&self, duration: Duration, renewable: bool) -> Lease {
        Lease { id: self.id.clone(), duration, renewable, issued: Instant::now() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aged(duration: Duration, elapsed: Duration, renewable: bool) -> Lease {
        Lease {
            id: "lease/x".into(),
            duration,
            renewable,
            issued: Instant::now() - elapsed,
        }
    }

    #[test]
    fn a_static_secret_has_no_lease_and_never_expires() {
        let lease = Lease::none();

        assert!(!lease.exists());
        assert!(!lease.is_expired());
        assert!(!lease.should_renew());
        assert_eq!(lease.renew_after(), Duration::MAX);
    }

    #[test]
    fn renewal_is_due_at_two_thirds_of_the_lease() {
        let hour = Duration::from_secs(3600);

        // Fresh: two thirds of an hour still to go, give or take the moment
        // this test takes to run.
        let fresh = aged(hour, Duration::ZERO, true);
        let due = fresh.renew_after().as_secs();
        assert!((2390..=2400).contains(&due), "expected about 2400s, got {due}");

        // Past the two-thirds mark: due now.
        assert!(aged(hour, Duration::from_secs(2500), true).should_renew());
        assert!(!aged(hour, Duration::from_secs(1000), true).should_renew());
    }

    #[test]
    fn a_third_of_the_lease_is_left_as_room_to_fail() {
        // The property that makes the fraction worth having: at the moment
        // renewal becomes due there is still real time left to retry in.
        let hour = Duration::from_secs(3600);
        let due = aged(hour, Duration::from_secs(2400), true);

        assert!(due.should_renew());
        assert!(due.remaining() >= Duration::from_secs(1150), "no room left to retry");
        assert!(!due.is_expired());
    }

    #[test]
    fn a_lease_that_cannot_be_renewed_is_never_due() {
        // Asking to renew something Vault will not renew is a request that can
        // only fail, and doing it on a timer is a request that fails forever.
        let lease = aged(Duration::from_secs(60), Duration::from_secs(59), false);

        assert!(!lease.should_renew());
        assert_eq!(lease.renew_after(), Duration::MAX);
    }

    #[test]
    fn a_very_short_lease_is_due_immediately_rather_than_never() {
        let lease = aged(Duration::from_secs(2), Duration::from_secs(5), true);

        assert!(lease.should_renew());
        assert!(lease.is_expired());
    }

    #[test]
    fn remaining_saturates_instead_of_underflowing() {
        // Duration subtraction panics on overflow, and an expired lease is the
        // ordinary case, not an exceptional one.
        let lease = aged(Duration::from_secs(10), Duration::from_secs(1000), true);

        assert_eq!(lease.remaining(), Duration::ZERO);
        assert!(lease.is_expired());
    }

    #[test]
    fn renewing_keeps_the_id_and_resets_the_clock() {
        let lease = aged(Duration::from_secs(3600), Duration::from_secs(3000), true);
        let renewed = lease.renewed(Duration::from_secs(3600), true);

        assert_eq!(renewed.id, lease.id, "renewal extends a lease, it does not issue one");
        assert!(!renewed.should_renew(), "the clock restarted");
        assert!(renewed.remaining() > Duration::from_secs(3500));
    }
}
