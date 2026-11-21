use super::{
    document::{writer::XmlDocumentRoot, PrintTicketDocument},
    DEFAULT_PRINT_TICKET_XML,
};
use crate::{
    printer::PrinterDevice,
    utils::{stream::read_com_stream, wchar},
};
use scopeguard::defer;
use std::ptr;
use thiserror::Error;
use windows::{
    core::{BSTR, PCWSTR},
    Win32::{
        Graphics::{
            Gdi::{DEVMODEA, DEVMODEW, DM_IN_BUFFER, DM_OUT_BUFFER},
            Printing::{
                ClosePrinter, DocumentPropertiesW, GetPrinterW, OpenPrinterW,
                PrintTicket::{
                    kPTJobScope, kUserDefaultDevmode, PTCloseProvider,
                    PTConvertDevModeToPrintTicket, PTConvertPrintTicketToDevMode, PTOpenProvider,
                    PTReleaseMemory,
                },
                PRINTER_HANDLE, PRINTER_INFO_9W,
            },
        },
        UI::{Shell::SHCreateMemStream, WindowsAndMessaging::IDOK},
    },
};

#[derive(Clone, Debug)]
/// Represents a print ticket.
pub struct PrintTicket {
    pub(crate) xml: Vec<u8>,
}

impl Default for PrintTicket {
    fn default() -> Self {
        Self {
            xml: DEFAULT_PRINT_TICKET_XML.into(),
        }
    }
}

#[derive(Error, Debug)]
/// Represents an error occurred while converting print ticket to dev mode.
pub enum ToDevModeError {
    /// Failed to open print ticket provider.
    #[error("Failed to open print ticket provider")]
    OpenProviderFailed(#[source] windows::core::Error),
    /// Stream not allocated.
    #[error("Stream not allocated")]
    StreamNotAllocated,
    /// Failed to convert print ticket to dev mode.
    #[error("Failed to convert print ticket to dev mode")]
    ConvertPrintTicketToDevModeFailed(String, #[source] windows::core::Error),
    /// Failed to open printer.
    #[error("Failed to open printer")]
    FailedToOpenPrinter(#[source] windows::core::Error),
    /// Failed to correct dev mode via [`DocumentProperties`].
    ///
    /// [`DocumentProperties`]: https://learn.microsoft.com/en-us/windows/win32/printdocs/documentproperties
    #[error("Failed to correct dev mode via DocumentProperties")]
    FailedToCorrectDevMode,
}

#[derive(Error, Debug)]
/// Represents an error occurred while reading the user's default print ticket.
pub enum UserDefaultError {
    /// Failed to open printer.
    #[error("Failed to open printer")]
    FailedToOpenPrinter(#[source] windows::core::Error),
    /// Failed to read the printer's settings via [`DocumentProperties`].
    ///
    /// [`DocumentProperties`]: https://learn.microsoft.com/en-us/windows/win32/printdocs/documentproperties
    #[error("Failed to read the printer's settings via DocumentProperties")]
    FailedToReadDevMode,
    /// Failed to convert the settings to a print ticket ([`PrintTicket::from_dev_mode`]).
    #[error("Failed to convert the settings to a print ticket")]
    FromDevMode(#[source] FromDevModeError),
}

impl PrintTicket {
    /// The signed-in user's own default print ticket for the printer: the settings saved in its
    /// Printing Preferences for this user ([`GetPrinter`] level 9), else the printer's defaults,
    /// settled by the driver ([`DocumentProperties`]) and converted at job scope
    /// ([`PrintTicket::from_dev_mode`]).
    ///
    /// The Print Ticket API completes a ticket merged onto the empty ticket, and answers a
    /// capabilities request without one, from the printer's Printing Defaults, which can differ
    /// from what the user set: a label printer's stock and, through the driver's private features
    /// (a Zebra ZDesigner driver's `PageDevmodeSnapshot`), its darkness or cutter. Use this as the
    /// base instead. Level 9 is only served to an interactive session; elsewhere this falls back
    /// to the printer's defaults.
    ///
    /// [`GetPrinter`]: https://learn.microsoft.com/en-us/windows/win32/printdocs/getprinter
    /// [`DocumentProperties`]: https://learn.microsoft.com/en-us/windows/win32/printdocs/documentproperties
    pub fn user_default(device: &PrinterDevice) -> Result<Self, UserDefaultError> {
        let printer_name = wchar::to_wide_chars(device.os_name());
        unsafe {
            let printer_handle = {
                let mut printer_handle = PRINTER_HANDLE::default();
                OpenPrinterW(
                    PCWSTR(printer_name.as_ptr()),
                    ptr::addr_of_mut!(printer_handle),
                    None,
                )
                .map_err(UserDefaultError::FailedToOpenPrinter)?;
                printer_handle
            };
            defer! {
                let _ = ClosePrinter(printer_handle);
            }

            // The per-user settings, when there are any and this session is served them. The
            // read is made again at the size it asks for when they grew after the size query (the
            // user saving Printing Preferences meanwhile), a few times at most, and they are used
            // only when they lie wholly within what was read.
            let mut needed = 0u32;
            let _ = GetPrinterW(printer_handle, 9, None, ptr::addr_of_mut!(needed));
            // Lives until the end: `per_user` points into it.
            let mut info: Vec<u8>;
            let mut per_user: *const DEVMODEW = ptr::null();
            for _ in 0..3 {
                let size = needed as usize;
                if size < std::mem::size_of::<PRINTER_INFO_9W>() || size > MAX_PRINTER_INFO {
                    break;
                }
                info = vec![0u8; size];
                if GetPrinterW(
                    printer_handle,
                    9,
                    Some(&mut info),
                    ptr::addr_of_mut!(needed),
                )
                .is_ok()
                {
                    let at = ptr::read_unaligned(info.as_ptr() as *const PRINTER_INFO_9W).pDevMode;
                    if (at as usize)
                        .checked_sub(info.as_ptr() as usize)
                        .is_some_and(|offset| dev_mode_within(&info, offset))
                    {
                        per_user = at;
                    }
                    break;
                }
                if needed as usize <= size {
                    break;
                }
            }

            let size = DocumentPropertiesW(
                None,
                printer_handle,
                PCWSTR(printer_name.as_ptr()),
                None,
                None,
                0,
            );
            if size <= 0 {
                return Err(UserDefaultError::FailedToReadDevMode);
            }
            // Workaround for buggy printer drivers, as in `to_dev_mode`.
            let mut buffer = vec![0u8; size as usize * 2 + 8192];
            let (input, mode) = if per_user.is_null() {
                (None, DM_OUT_BUFFER.0)
            } else {
                (Some(per_user), (DM_IN_BUFFER | DM_OUT_BUFFER).0)
            };
            if DocumentPropertiesW(
                None,
                printer_handle,
                PCWSTR(printer_name.as_ptr()),
                Some(buffer.as_mut_ptr() as *mut _),
                input,
                mode,
            ) != IDOK.0
            {
                return Err(UserDefaultError::FailedToReadDevMode);
            }
            let size = dev_mode_size(&buffer).ok_or(UserDefaultError::FailedToReadDevMode)?;
            Self::from_dev_mode(device, &buffer[..size]).map_err(UserDefaultError::FromDevMode)
        }
    }

    /// Create a new print ticket from the given XML.
    pub fn from_xml(xml: impl Into<Vec<u8>>) -> Self {
        Self { xml: xml.into() }
    }

    /// Into the XML of the print ticket.
    pub fn into_xml(self) -> Vec<u8> {
        self.xml
    }

    /// Get the XML of the print ticket.
    pub fn get_xml(&self) -> &[u8] {
        &self.xml
    }

    /// Convert the print ticket to [`DEVMODE`] data.
    ///
    /// [`DEVMODE`]: https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-devmodew
    pub fn to_dev_mode(&self, device: &PrinterDevice) -> Result<Vec<u8>, ToDevModeError> {
        unsafe {
            let provider =
                PTOpenProvider(PCWSTR(wchar::to_wide_chars(device.os_name()).as_ptr()), 1)
                    .map_err(ToDevModeError::OpenProviderFailed)?;
            defer! {
                let _ = PTCloseProvider(provider);
            }

            let stream = SHCreateMemStream(Some(self.get_xml()))
                .ok_or(ToDevModeError::StreamNotAllocated)?;

            let mut dev_mode_size = 0;
            let mut dev_mode_data = std::ptr::null_mut();
            let mut error_message = BSTR::default();
            PTConvertPrintTicketToDevMode(
                provider,
                &stream,
                kUserDefaultDevmode,
                kPTJobScope,
                ptr::addr_of_mut!(dev_mode_size),
                ptr::addr_of_mut!(dev_mode_data),
                Some(&mut error_message),
            )
            .map_err(|win32_error| {
                ToDevModeError::ConvertPrintTicketToDevModeFailed(
                    error_message.to_string(),
                    win32_error,
                )
            })?;
            defer! {
                let _ = PTReleaseMemory(dev_mode_data as *mut _);
            }

            let printer_name = wchar::to_wide_chars(device.os_name());
            let printer_handle = {
                let mut printer_handle = PRINTER_HANDLE::default();
                OpenPrinterW(
                    PCWSTR(printer_name.as_ptr()),
                    ptr::addr_of_mut!(printer_handle),
                    None,
                )
                .map_err(ToDevModeError::FailedToOpenPrinter)?;
                printer_handle
            };
            defer! {
                let _ = ClosePrinter(printer_handle);
            }

            let mut buffer_size = DocumentPropertiesW(
                None,
                printer_handle,
                PCWSTR(printer_name.as_ptr()),
                None,
                None,
                0,
            );

            // Workaround for buggy printer drivers
            // See also: https://chromium.googlesource.com/chromium/src/+/refs/tags/124.0.6347.1/printing/backend/win_helper.cc#586
            buffer_size = buffer_size * 2 + 8192;

            let mut buffer = vec![0u8; buffer_size as usize];
            if DocumentPropertiesW(
                None,
                printer_handle,
                PCWSTR(printer_name.as_ptr()),
                Some(buffer.as_mut_ptr() as *mut _),
                Some(dev_mode_data as *mut _),
                (DM_IN_BUFFER | DM_OUT_BUFFER).0,
            ) != IDOK.0
            {
                return Err(ToDevModeError::FailedToCorrectDevMode);
            }

            Ok(buffer)
        }
    }

    /// Convert [`DEVMODE`] data, such as the settings a printer driver's own dialog returns, to a
    /// job-scope print ticket. `dev_mode` must be for `device`: Windows may replace the settings
    /// of another printer's driver with defaults. Bytes after the DEVMODE's own size
    /// (`dmSize + dmDriverExtra`) are ignored, so the buffer [`PrintTicket::to_dev_mode`] returns
    /// can be passed back as it is.
    ///
    /// [`DEVMODE`]: https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-devmodew
    pub fn from_dev_mode(
        device: &PrinterDevice,
        dev_mode: &[u8],
    ) -> Result<Self, FromDevModeError> {
        let size = dev_mode_size(dev_mode).ok_or(FromDevModeError::InvalidDevMode)?;
        // DEVMODEW's fields need their natural alignment, which a byte slice doesn't promise. An
        // older, shorter DEVMODEW is padded with zeros to the whole structure, so no field read
        // through it lies outside the buffer.
        let mut aligned = vec![0u64; size.max(std::mem::size_of::<DEVMODEW>()).div_ceil(8)];
        for (word, bytes) in aligned.iter_mut().zip(dev_mode[..size].chunks(8)) {
            let mut padded = [0u8; 8];
            padded[..bytes.len()].copy_from_slice(bytes);
            *word = u64::from_ne_bytes(padded);
        }
        unsafe {
            let provider =
                PTOpenProvider(PCWSTR(wchar::to_wide_chars(device.os_name()).as_ptr()), 1)
                    .map_err(FromDevModeError::OpenProviderFailed)?;
            defer! {
                let _ = PTCloseProvider(provider);
            }
            let stream = SHCreateMemStream(None).ok_or(FromDevModeError::StreamNotAllocated)?;
            PTConvertDevModeToPrintTicket(
                provider,
                size as u32,
                aligned.as_ptr() as *const DEVMODEA,
                kPTJobScope,
                &stream,
            )
            .map_err(FromDevModeError::ConvertDevModeToPrintTicketFailed)?;
            let xml = read_com_stream(&stream).map_err(FromDevModeError::ReadPrintTicketFailed)?;
            Ok(Self { xml })
        }
    }
}

/// Offsets inside DEVMODEW: `dmSize` and `dmDriverExtra` follow the 32-character device name and
/// two version words.
const DM_SIZE_AT: usize = 68;
const DM_DRIVER_EXTRA_AT: usize = 70;
/// The smallest public part of a DEVMODEW: the members up to `dmFields`.
const DM_MIN_SIZE: usize = 76;

/// The size of the DEVMODE at the start of `bytes` (`dmSize + dmDriverExtra`); `None` when its
/// public part ends before `dmFields` does or `bytes` doesn't hold all of it.
fn dev_mode_size(bytes: &[u8]) -> Option<usize> {
    let word = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|w| usize::from(u16::from_le_bytes([w[0], w[1]])))
    };
    let public = word(DM_SIZE_AT)?;
    let size = public + word(DM_DRIVER_EXTRA_AT)?;
    (public >= DM_MIN_SIZE && size <= bytes.len()).then_some(size)
}

#[derive(Error, Debug)]
/// Represents an error occurred while converting dev mode to print ticket.
pub enum FromDevModeError {
    /// The data is not a whole DEVMODE: its public part ends before `dmFields`, or the data is
    /// shorter than its `dmSize` and `dmDriverExtra` say.
    #[error("Invalid dev mode")]
    InvalidDevMode,
    /// Failed to open print ticket provider.
    #[error("Failed to open print ticket provider")]
    OpenProviderFailed(#[source] windows::core::Error),
    /// Stream not allocated.
    #[error("Stream not allocated")]
    StreamNotAllocated,
    /// Failed to convert dev mode to print ticket.
    #[error("Failed to convert dev mode to print ticket")]
    ConvertDevModeToPrintTicketFailed(#[source] windows::core::Error),
    /// Failed to read the converted print ticket.
    #[error("Failed to read print ticket")]
    ReadPrintTicketFailed(#[source] windows::core::Error),
}

impl From<PrintTicketDocument> for PrintTicket {
    fn from(value: PrintTicketDocument) -> Self {
        Self {
            xml: value.to_xml(),
        }
    }
}

/// The most a GetPrinter level 9 answer is read at: larger ones are not a user's settings.
const MAX_PRINTER_INFO: usize = 1 << 20;

/// Whether `buf` holds a whole DEVMODEW at `offset`, as [`dev_mode_size`] reads one.
fn dev_mode_within(buf: &[u8], offset: usize) -> bool {
    buf.get(offset..).and_then(dev_mode_size).is_some()
}

#[cfg(test)]
mod tests {
    use super::{dev_mode_size, dev_mode_within, FromDevModeError, PrintTicket};
    use crate::test_utils::null_device;

    const LANDSCAPE: &str = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" version="1" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOrientation">
        <psf:Option name="psk:Landscape" />
    </psf:Feature>
</psf:PrintTicket>"#;

    /// A DEVMODE header whose `dmSize` and `dmDriverExtra` are `size` and `extra`, `len` bytes long.
    fn header(size: u16, extra: u16, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        bytes[68..70].copy_from_slice(&size.to_le_bytes());
        bytes[70..72].copy_from_slice(&extra.to_le_bytes());
        bytes
    }

    #[test]
    fn dev_mode_size_is_the_public_and_private_parts() {
        assert_eq!(dev_mode_size(&header(220, 0, 220)), Some(220));
        assert_eq!(dev_mode_size(&header(220, 100, 320)), Some(320));
        // Spare bytes after the DEVMODE, as `to_dev_mode` returns it.
        assert_eq!(dev_mode_size(&header(220, 100, 9000)), Some(320));
        // The smallest public part: the members up to `dmFields`.
        assert_eq!(dev_mode_size(&header(76, 0, 76)), Some(76));
    }

    #[test]
    fn dev_mode_size_refuses_a_partial_dev_mode() {
        assert_eq!(dev_mode_size(&[]), None);
        assert_eq!(dev_mode_size(&[0u8; 71]), None);
        // Shorter than its own sizes say.
        assert_eq!(dev_mode_size(&header(220, 100, 319)), None);
        // A public part that ends before `dmFields` does.
        assert_eq!(dev_mode_size(&header(72, 0, 72)), None);
        assert_eq!(dev_mode_size(&header(75, 0, 76)), None);
    }

    #[test]
    fn a_dev_mode_converts_back_to_its_ticket() {
        let device = null_device::thread_local();
        let dev_mode = PrintTicket::from_xml(LANDSCAPE)
            .to_dev_mode(&device)
            .unwrap();
        let ticket = PrintTicket::from_dev_mode(&device, &dev_mode).unwrap();
        let xml = String::from_utf8(ticket.into_xml()).unwrap();
        assert!(xml.contains("PrintTicket"), "{xml}");
        assert!(xml.contains("Landscape"), "{xml}");
    }

    #[test]
    fn a_partial_dev_mode_is_refused_before_the_driver_reads_it() {
        let device = null_device::thread_local();
        let dev_mode = PrintTicket::default().to_dev_mode(&device).unwrap();
        let size = dev_mode_size(&dev_mode).unwrap();
        let result = PrintTicket::from_dev_mode(&device, &dev_mode[..size - 1]);
        assert!(matches!(result, Err(FromDevModeError::InvalidDevMode)));
    }

    /// A GetPrinter level 9 answer: a header, then a DEVMODEW with `size` public and `extra`
    /// private bytes.
    fn answer(header: usize, size: u16, extra: u16) -> Vec<u8> {
        let mut buf = vec![0u8; header + usize::from(size) + usize::from(extra)];
        buf[header + 68..header + 70].copy_from_slice(&size.to_le_bytes());
        buf[header + 70..header + 72].copy_from_slice(&extra.to_le_bytes());
        buf
    }

    #[test]
    fn the_users_settings_are_used_only_when_wholly_within_the_answer() {
        let buf = answer(8, 220, 1200);
        assert!(dev_mode_within(&buf, 8));
        assert!(
            !dev_mode_within(&buf[..buf.len() - 1], 8),
            "a private part cut short"
        );
        assert!(!dev_mode_within(&buf, buf.len()), "settings at the end");
        assert!(
            !dev_mode_within(&buf, usize::MAX),
            "an offset past any buffer"
        );
        assert!(!dev_mode_within(&[], 0), "no answer");
        assert!(
            !dev_mode_within(&answer(8, 75, 0), 8),
            "too small to be settings"
        );
        assert!(
            dev_mode_within(&answer(0, 220, u16::MAX), 0),
            "the largest private part"
        );
    }

    #[test]
    fn the_user_default_ticket_is_a_complete_job_ticket() {
        let device = null_device::thread_local();
        let ticket = PrintTicket::user_default(&device).unwrap();
        let xml = String::from_utf8(ticket.into_xml()).unwrap();
        assert!(
            xml.contains("PrintTicket") && xml.contains("PageMediaSize"),
            "{xml}"
        );
    }
}
