#![allow(dead_code)]

use lazy_static::lazy_static;
use numext_fixed_hash::H256;

pub type Message = H256;

lazy_static! {
    pub static ref SECP256K1: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::new();
}

mod error;
mod generator;
mod privkey;
mod pubkey;
mod signature;

pub use self::error::Error;
pub use self::generator::Generator;
pub use self::privkey::Privkey;
pub use self::pubkey::Pubkey;
pub use self::signature::Signature;

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{self, Rng};

    fn random_message() -> Message {
        let mut message = Message::default();
        let mut rng = rand::thread_rng();
        rng.fill(message.as_mut());
        message
    }

    #[test]
    fn test_sign_verify() {
        let gen = Generator::new();
        let (privkey, pubkey) = gen.random_keypair().unwrap();
        let message = random_message();
        let signature = privkey.sign_recoverable(&message).unwrap();
        assert!(pubkey.verify(&message, &signature).is_ok());
    }

    #[test]
    fn test_recover() {
        let gen = Generator::new();
        let (privkey, pubkey) = gen.random_keypair().unwrap();
        let message = random_message();
        let signature = privkey.sign_recoverable(&message).unwrap();
        assert_eq!(pubkey, signature.recover(&message).unwrap());
    }

}
