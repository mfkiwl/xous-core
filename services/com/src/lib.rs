#![cfg_attr(target_os = "none", no_std)]

use core::convert::TryInto;

/// This is the API that other servers use to call the COM. Read this code as if you
/// are calling these functions inside a different process.
pub mod api;

use api::BattStats;
use xous::{send_message, Error, CID};
use xous_ipc::String;

pub fn power_off_soc(cid: CID) -> Result<(), xous::Error> {
    send_message(cid, api::Opcode::PowerOffSoc.into()).map(|_| ())
}

pub fn get_batt_stats(cid: CID) -> Result<BattStats, xous::Error> {
    let response = send_message(cid, api::Opcode::BattStats.into())?;
    if let xous::Result::Scalar2(upper, lower) = response {
        let raw_stats: [usize; 2] = [lower, upper];
        Ok(raw_stats.into())
    } else {
        panic!("unexpected return value: {:#?}", response);
    }
}

pub fn get_batt_stats_nb(cid: CID) -> Result<(), xous::Error> {
    send_message(cid, api::Opcode::BattStatsNb.into()).map(|_| ())
}

pub fn get_wf200_fw_rev(cid: CID) -> Result<(u8, u8, u8), xous::Error> {
    let response = send_message(cid, api::Opcode::Wf200Rev.into())?;
    if let xous::Result::Scalar1(rev) = response {
        Ok(((rev >> 16) as u8, (rev >> 8) as u8, rev as u8))
    } else {
        panic!("unexpected return value: {:#?}", response);
    }
}

pub fn get_ec_git_rev(cid: CID) -> Result<(u32, bool), Error> {
    let response = send_message(cid, api::Opcode::EcGitRev.into())?;
    if let xous::Result::Scalar2(rev, dirty) = response {
        let dirtybool: bool;
        if dirty == 0 {
            dirtybool = false;
        } else {
            dirtybool = true;
        }
        Ok((rev as u32, dirtybool))
    } else {
        panic!("unexpected return value: {:#?}", response);
    }
}

pub fn send_pds_line(cid: CID, s: &String<512>) -> Result<(), Error> {
    use core::fmt::Write;
    let mut clone_s: String<512> = String::new();
    write!(clone_s, "{}", s.as_str().unwrap()).map_err(|_| xous::Error::AccessDenied)?;
    let request = api::Opcode::Wf200PdsLine(clone_s);
    let mut writer = rkyv::ser::serializers::BufferSerializer::new(xous::XousBuffer::new(4096));
    let pos = {
        use rkyv::ser::Serializer;
        writer.serialize_value(&request).expect("COM: couldn't archive PDS line request")
    };
    let xous_buffer = writer.into_inner();

    xous_buffer.lend(cid, pos as u32).expect("COM: PDS line request failure");
    Ok(())
}

pub fn get_rx_stats_agent(cid: CID) -> Result<(), Error> {
    send_message(cid, api::Opcode::RxStatsAgent.into()).map(|_| ())
}


// event relay request API
pub fn request_battstat_events(name: &str, com_conn: xous::CID) -> Result<xous::Result, xous::Error> {
    let s = String::<64>::from_str(name);
    let request = api::Opcode::RegisterBattStatsListener(s);
    let mut writer = rkyv::ser::serializers::BufferSerializer::new(xous::XousBuffer::new(4096));
    let pos = {
        use rkyv::ser::Serializer;
        writer.serialize_value(&request).expect("COM: couldn't archive battstat request")
    };
    let xous_buffer = writer.into_inner();

    xous_buffer
       .lend(com_conn, pos.try_into().unwrap())
}

// note to future self: add other event listener registrations (such as network events) here
