//! Read-only Windows VPN route checks for the explicit Moodle CLI command.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ptr::{NonNull, addr_of, null_mut},
    slice,
};
use windows_sys::Win32::{
    NetworkManagement::{
        IpHelper::{
            FreeMibTable, GetBestInterfaceEx, GetIfTable2, GetIpInterfaceTable,
            GetUnicastIpAddressTable, MIB_IF_ROW2, MIB_IF_TABLE2, MIB_IPINTERFACE_ROW,
            MIB_IPINTERFACE_TABLE, MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE,
        },
        Ndis::IfOperStatusUp,
    },
    Networking::WinSock::{AF_INET, IN_ADDR, IN_ADDR_0, IpDadStatePreferred, SOCKADDR_IN},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowsMoodleVpn {
    interface: u32,
    pub(crate) source: Ipv4Addr,
}

impl WindowsMoodleVpn {
    pub(crate) fn connected() -> Option<Self> {
        let adapters: Table<MIB_IF_TABLE2> = Table::read(|output| unsafe { GetIfTable2(output) })?;
        let adapters_ptr = adapters.0.as_ptr();
        let adapters = unsafe {
            slice::from_raw_parts(
                addr_of!((*adapters_ptr).Table).cast::<MIB_IF_ROW2>(),
                (*adapters_ptr).NumEntries as usize,
            )
        };
        let ip_interfaces: Table<MIB_IPINTERFACE_TABLE> =
            Table::read(|output| unsafe { GetIpInterfaceTable(AF_INET, output) })?;
        let ip = ip_interfaces.0.as_ptr();
        let rows = unsafe {
            slice::from_raw_parts(
                addr_of!((*ip).Table).cast::<MIB_IPINTERFACE_ROW>(),
                (*ip).NumEntries as usize,
            )
        };
        // NDIS exposes Fortinet filter rows as up even without an IP interface.
        let mut available = adapters.iter().filter(|adapter| {
            let description = String::from_utf16_lossy(
                adapter
                    .Description
                    .split(|character| *character == 0)
                    .next()
                    .unwrap_or(&[]),
            )
            .to_ascii_lowercase();
            adapter.OperStatus == IfOperStatusUp
                && description.contains("fortinet")
                && description.contains("vpn")
                && rows
                    .iter()
                    .any(|row| row.InterfaceIndex == adapter.InterfaceIndex && row.Connected)
        });
        let interface = available.next()?.InterfaceIndex;
        if interface == 0 || available.next().is_some() {
            return None;
        }
        // Reqwest binds the source address on Windows. With strong-host sending
        // on other interfaces, that source cannot be used through those interfaces.
        if rows
            .iter()
            .any(|row| row.InterfaceIndex != interface && row.WeakHostSend)
        {
            return None;
        }

        let unicast: Table<MIB_UNICASTIPADDRESS_TABLE> =
            Table::read(|output| unsafe { GetUnicastIpAddressTable(AF_INET, output) })?;
        let unicast_ptr = unicast.0.as_ptr();
        let rows = unsafe {
            slice::from_raw_parts(
                addr_of!((*unicast_ptr).Table).cast::<MIB_UNICASTIPADDRESS_ROW>(),
                (*unicast_ptr).NumEntries as usize,
            )
        };
        let mut sources = rows.iter().filter(|row| {
            row.InterfaceIndex == interface
                && row.DadState == IpDadStatePreferred
                && !row.SkipAsSource
                && unsafe { row.Address.si_family } == AF_INET
        });
        let source = Ipv4Addr::from(
            unsafe { sources.next()?.Address.Ipv4.sin_addr.S_un.S_addr }.to_ne_bytes(),
        );
        if source.is_unspecified()
            || source.is_loopback()
            || source.is_link_local()
            || sources.next().is_some()
            || rows.iter().any(|row| {
                row.InterfaceIndex != interface
                    && unsafe {
                        row.Address.si_family == AF_INET
                            && row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes()
                                == source.octets()
                    }
            })
        {
            return None;
        }

        Some(Self { interface, source })
    }

    pub(crate) fn select(self, answers: &[SocketAddr]) -> Option<SocketAddr> {
        // Confirm address ownership and the strong-host conditions again after DNS.
        if Self::connected()? != self {
            return None;
        }
        answers.iter().copied().find(|answer| {
            let IpAddr::V4(address) = answer.ip() else {
                return false;
            };
            let socket = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 443u16.to_be(),
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(address.octets()),
                    },
                },
                sin_zero: [0; 8],
            };
            let mut index = 0;
            // Route-table lookup only. It does not resolve names or open a socket.
            let status = unsafe { GetBestInterfaceEx(addr_of!(socket).cast(), &mut index) };
            status == 0 && index == self.interface
        })
    }
}

// The Get*Table APIs allocate variable-length arrays and transfer ownership.
struct Table<T>(NonNull<T>);

impl<T> Table<T> {
    fn read(get: impl FnOnce(*mut *mut T) -> u32) -> Option<Self> {
        let mut table = null_mut();
        if get(&mut table) != 0 {
            return None;
        }
        NonNull::new(table).map(Self)
    }
}

impl<T> Drop for Table<T> {
    fn drop(&mut self) {
        unsafe { FreeMibTable(self.0.as_ptr().cast()) };
    }
}
