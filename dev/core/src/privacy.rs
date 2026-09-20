//! Peer identity distribution and host-side resolution of private addresses.
use aes::{Aes128, cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray}};
use crate::{hci::BdAddr, smp::SmpError};

#[derive(Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// SMP wire order (least significant octet first). Never log this key.
    pub irk: [u8; 16],
    pub address: BdAddr,
    pub address_type: u8,
}

impl std::fmt::Debug for PeerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerIdentity").field("address", &self.address)
            .field("address_type", &self.address_type).finish_non_exhaustive()
    }
}

impl PeerIdentity {
    pub fn from_pdus(key: &[u8], address: &[u8]) -> Result<Self, SmpError> {
        if key.len() != 17 || key[0] != 0x08 || address.len() != 8 || address[0] != 0x09
            || address[1] > 1 || (address[1] == 1 && address[7] & 0xC0 != 0xC0) {
            return Err(SmpError::Malformed("identity distribution"));
        }
        Ok(Self { irk: key[1..].try_into().unwrap(),
            address: BdAddr(address[2..].try_into().unwrap()), address_type: address[1] })
    }

    pub fn matches(&self, address: BdAddr, address_type: u8) -> bool {
        if address == self.address && address_type == self.address_type { return true; }
        if address_type != 1 || address.0[5] & 0xC0 != 0x40 || self.irk == [0; 16] { return false; }
        let mut key = self.irk;
        key.reverse();
        let cipher = Aes128::new(GenericArray::from_slice(&key));
        let mut block = [0u8; 16];
        block[13..].copy_from_slice(&[address.0[5], address.0[4], address.0[3]]);
        cipher.encrypt_block(GenericArray::from_mut_slice(&mut block));
        [block[15], block[14], block[13]] == address.0[..3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_core_specification_d7_vector_in_wire_order() {
        let identity = PeerIdentity { irk: [0x9b,0x7d,0x39,0x0a,0xa6,0x10,0x10,0x34,0x05,0xad,0xc8,0x57,0xa3,0x34,0x02,0xec],
            address: BdAddr([0;6]), address_type: 0 };
        let rpa = BdAddr::parse("70:81:94:0D:FB:AA").unwrap();
        assert!(identity.matches(rpa, 1));
        assert!(!identity.matches(rpa, 0));
        assert!(!identity.matches(BdAddr::parse("70:81:94:0D:FB:AB").unwrap(), 1));
    }
    #[test]
    fn rejects_truncated_or_non_identity_address() {
        assert!(PeerIdentity::from_pdus(&[8;16], &[9;8]).is_err());
        let mut key = [0;17]; key[0] = 8;
        assert!(PeerIdentity::from_pdus(&key, &[9,1,0,0,0,0,0,0x40]).is_err());
        assert!(PeerIdentity::from_pdus(&key, &[9,1,1,2,3,4,5,0xc0]).is_ok());
    }
}
