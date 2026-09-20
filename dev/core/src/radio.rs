//! Standard, read-only LE power telemetry. There is no portable HCI setter
//! for an active connection's absolute transmit power.
use crate::hci;
pub const READ_RANGE:u16=0x204b;
pub const READ_CONNECTION:u16=0x2076;
#[derive(Debug,Clone,Default)]
pub struct Power {
    pub phy:u8, pub min:Option<i8>, pub adapter_max:Option<i8>,
    pub current:Option<i8>, pub connection_max:Option<i8>, pub available:bool,
}
pub fn range_command()->Vec<u8>{hci::command(READ_RANGE,&[])}
pub fn connection_command(handle:u16,phy:u8)->Vec<u8>{
    let mut p=handle.to_le_bytes().to_vec();p.push(phy);hci::command(READ_CONNECTION,&p)
}
fn dbm(value:u8)->Option<i8>{let value=value as i8;(-127..=20).contains(&value).then_some(value)}
pub fn range(p:&[u8])->Option<(i8,i8)>{
    if p.len()!=3 || p[0]!=0{return None;}let min=dbm(p[1])?;let max=dbm(p[2])?;
    (min<=max).then_some((min,max))
}
pub fn connection(p:&[u8],handle:u16,phy:u8)->Option<(Option<i8>,Option<i8>)>{
    if p.len()!=6 || p[0]!=0 || u16::from_le_bytes([p[1],p[2]])!=handle || p[3]!=phy{return None;}
    Some((dbm(p[4]),dbm(p[5])))
}
#[cfg(test)]mod tests {
    use super::*;
    #[test]fn signed_power_and_sentinels(){assert_eq!(range(&[0,236,10]),Some((-20,10)));
        assert_eq!(connection(&[0,12,0,2,127,10],12,2),Some((None,Some(10))));
        assert_eq!(connection(&[0,12,0,2,126,127],12,2),Some((None,None)));}
    #[test]fn rejects_wrong_handle_phy_status_and_truncation(){
        for p in [&[0,13,0,2,4,10][..],&[0,12,0,1,4,10],&[1,12,0,2,4,10],&[0,12,0]]{assert!(connection(p,12,2).is_none());}
        assert!(range(&[0,10,236]).is_none());
    }
    #[test]fn uses_read_only_standard_commands(){assert_eq!(range_command(),[0x4b,0x20,0]);assert_eq!(connection_command(12,1),[0x76,0x20,3,12,0,1]);}
}
