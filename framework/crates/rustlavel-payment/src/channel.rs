//! How the customer pays.
//!
//! The Indonesian set, because that is where the first driver lives. The
//! enums are not closed — a driver for another market adds its variants — but
//! they are enums rather than strings so that a typo in a bank code is a
//! compile error rather than a charge nobody can pay.

use std::fmt;

/// Banks that issue virtual account numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bank {
    Bca,
    Bni,
    Bri,
    Cimb,
    Danamon,
    Mandiri,
    Permata,
    Bnc,
}

/// E-wallets a charge can be pushed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wallet {
    Dana,
    Ovo,
    ShopeePay,
    LinkAja,
    GoPay,
}

/// Retail counters that take cash against a payment code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Retail {
    Alfamart,
    Indomaret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// A bank transfer to a number that exists for this one charge.
    VirtualAccount(Bank),
    /// The national QR standard; any participating app can scan it.
    Qris,
    EWallet(Wallet),
    /// Cash, at a counter, against a code.
    Retail(Retail),
}

impl Channel {
    /// The stable lower-case name, for storage and for a driver's request.
    pub fn code(&self) -> String {
        match self {
            Channel::VirtualAccount(bank) => format!("va_{}", lower(&format!("{bank:?}"))),
            Channel::Qris => "qris".to_string(),
            Channel::EWallet(wallet) => format!("ewallet_{}", lower(&format!("{wallet:?}"))),
            Channel::Retail(store) => format!("retail_{}", lower(&format!("{store:?}"))),
        }
    }

    /// The inverse of [`Channel::code`].
    pub fn parse(code: &str) -> Option<Channel> {
        let code = code.trim().to_ascii_lowercase();
        if code == "qris" {
            return Some(Channel::Qris);
        }
        let (kind, name) = code.split_once('_')?;
        match kind {
            "va" => Some(Channel::VirtualAccount(match name {
                "bca" => Bank::Bca,
                "bni" => Bank::Bni,
                "bri" => Bank::Bri,
                "cimb" => Bank::Cimb,
                "danamon" => Bank::Danamon,
                "mandiri" => Bank::Mandiri,
                "permata" => Bank::Permata,
                "bnc" => Bank::Bnc,
                _ => return None,
            })),
            "ewallet" => Some(Channel::EWallet(match name {
                "dana" => Wallet::Dana,
                "ovo" => Wallet::Ovo,
                "shopeepay" => Wallet::ShopeePay,
                "linkaja" => Wallet::LinkAja,
                "gopay" => Wallet::GoPay,
                _ => return None,
            })),
            "retail" => Some(Channel::Retail(match name {
                "alfamart" => Retail::Alfamart,
                "indomaret" => Retail::Indomaret,
                _ => return None,
            })),
            _ => None,
        }
    }
}

fn lower(text: &str) -> String {
    text.to_ascii_lowercase()
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every channel survives the trip through its code, which is what is
    /// stored and what a driver sends.
    #[test]
    fn every_channel_round_trips_through_its_code() {
        let all = [
            Channel::VirtualAccount(Bank::Bca),
            Channel::VirtualAccount(Bank::Bni),
            Channel::VirtualAccount(Bank::Bri),
            Channel::VirtualAccount(Bank::Cimb),
            Channel::VirtualAccount(Bank::Danamon),
            Channel::VirtualAccount(Bank::Mandiri),
            Channel::VirtualAccount(Bank::Permata),
            Channel::VirtualAccount(Bank::Bnc),
            Channel::Qris,
            Channel::EWallet(Wallet::Dana),
            Channel::EWallet(Wallet::Ovo),
            Channel::EWallet(Wallet::ShopeePay),
            Channel::EWallet(Wallet::LinkAja),
            Channel::EWallet(Wallet::GoPay),
            Channel::Retail(Retail::Alfamart),
            Channel::Retail(Retail::Indomaret),
        ];
        for channel in all {
            assert_eq!(Channel::parse(&channel.code()), Some(channel), "{channel}");
        }
        assert_eq!(Channel::VirtualAccount(Bank::Bca).code(), "va_bca");
        assert_eq!(Channel::EWallet(Wallet::ShopeePay).code(), "ewallet_shopeepay");
    }

    #[test]
    fn an_unknown_code_is_none_rather_than_a_default() {
        assert_eq!(Channel::parse("va_unknownbank"), None);
        assert_eq!(Channel::parse("cash"), None);
        assert_eq!(Channel::parse(""), None);
    }
}
