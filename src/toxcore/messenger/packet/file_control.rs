/*! FileControl struct.
It is used to control transferring file to a friend.
*/

use nom::{le_u8, be_u64};

use crate::toxcore::binary_io::*;

/// Whether I am a sender or receiver of file data packet
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferDirection {
    /// I am a sender
    Send = 0,
    /// I am a receiver
    Receive
}

impl TransferDirection {
    /// Toggle direction
    pub fn toggle(&self) -> Self {
        if self == &TransferDirection::Send {
            TransferDirection::Receive
        } else {
            TransferDirection::Send
        }
    }
}

impl FromBytes for TransferDirection {
    named!(from_bytes<TransferDirection>,
        switch!(le_u8,
            0 => value!(TransferDirection::Send) |
            1 => value!(TransferDirection::Receive)
        )
    );
}

/// Control types for transferring file data
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlType {
    /// Accept a request of transferring file from a peer
    Accept,
    /// Pause transferring
    Pause ,
    /// Stop transferring and quit session
    Kill,
    /// Seek to position of file stream and holds seek parameter
    Seek(u64)
}

impl ToBytes for ControlType {
    fn to_bytes<'a>(&self, buf: (&'a mut [u8], usize)) -> Result<(&'a mut [u8], usize), GenError> {
        match self {
            ControlType::Accept => do_gen!(buf, gen_be_u8!(0x00)),
            ControlType::Pause => do_gen!(buf, gen_be_u8!(0x01)),
            ControlType::Kill => do_gen!(buf, gen_be_u8!(0x02)),
            ControlType::Seek(seek_param) => do_gen!(buf,
                gen_be_u8!(0x03) >>
                gen_be_u64!(*seek_param))
        }
    }
}

impl FromBytes for ControlType {
    named!(from_bytes<ControlType>,
        switch!(le_u8,
            0 => value!(ControlType::Accept) |
            1 => value!(ControlType::Pause) |
            2 => value!(ControlType::Kill) |
            3 => do_parse!(
                seek_param: be_u64 >>
                (ControlType::Seek(seek_param)))
        )
    );
}

/** FileControl is a struct that holds info to handle transferring file to a friend.

This packet is used to control transferring sender's file to a friend.
If a peer of connection wants to pause, kill, seek or accept transferring file, it would use this packet.

Serialized form:

Length    | Content
--------- | ------
`1`       | `0x51`
`1`       | Whether it is sending or receiving, 0 = sender, 1 = receiver
`1`       | `file_id`
`1`       | Control type: 0 = accept, 1 = pause, 2 = kill, 3 = seek
`8`       | Seek parameter which is only included when `control type` is seek(3)

*/

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileControl {
    /// Send or receive.
    pub transfer_direction: TransferDirection,
    /// Can not exceed 255
    pub file_id: u8,
    /// Kind of file transfer control.
    pub control_type: ControlType,
}

impl FromBytes for FileControl {
    named!(from_bytes<FileControl>, do_parse!(
        tag!("\x51") >>
        transfer_direction: call!(TransferDirection::from_bytes) >>
        file_id: le_u8 >>
        control_type: call!(ControlType::from_bytes) >>
        (FileControl {
            transfer_direction,
            file_id,
            control_type,
        })
    ));
}

impl ToBytes for FileControl {
    fn to_bytes<'a>(&self, buf: (&'a mut [u8], usize)) -> Result<(&'a mut [u8], usize), GenError> {
        do_gen!(buf,
            gen_be_u8!(0x51) >>
            gen_be_u8!(self.transfer_direction as u8) >>
            gen_be_u8!(self.file_id) >>
            gen_call!(|buf, control_type| ControlType::to_bytes(control_type, buf), &self.control_type)
    )}
}

impl FileControl {
    /// Create new FileControl object.
    pub fn new(transfer_direction: TransferDirection, file_id: u8, control_type: ControlType) -> Self {
        FileControl {
            transfer_direction,
            file_id,
            control_type,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    encode_decode_test!(
        file_control_encode_decode,
        FileControl::new(TransferDirection::Send, 1, ControlType::Seek(100))
    );
}
