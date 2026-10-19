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
    #[error("Failed to read the converted print ticket")]
    ReadTicketFailed(#[source] windows::core::Error),
}

impl PrintTicket {
    /// The signed-in user's own default print ticket for the printer: the settings saved in its
    /// Printing Preferences for this user ([`GetPrinter`] level 9), else the printer's defaults,
    /// settled by the driver ([`DocumentProperties`]) and converted at job scope.
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

            // The per-user settings, when there are any and this session is served them.
            let mut needed = 0u32;
            let _ = GetPrinterW(printer_handle, 9, None, ptr::addr_of_mut!(needed));
            let mut info = vec![0u8; needed as usize];
            let per_user: *const DEVMODEW = if info.len() >= std::mem::size_of::<PRINTER_INFO_9W>()
                && GetPrinterW(
                    printer_handle,
                    9,
                    Some(&mut info),
                    ptr::addr_of_mut!(needed),
                )
                .is_ok()
            {
                ptr::read_unaligned(info.as_ptr() as *const PRINTER_INFO_9W).pDevMode
            } else {
                ptr::null()
            };

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
            // dmSize and dmDriverExtra: the public and private parts of the DEVMODE.
            let field = |at: usize| u32::from(u16::from_le_bytes([buffer[at], buffer[at + 1]]));
            let dev_mode_size = field(68) + field(70);
            if dev_mode_size == 0 || dev_mode_size as usize > buffer.len() {
                return Err(UserDefaultError::FailedToReadDevMode);
            }

            let provider = PTOpenProvider(PCWSTR(printer_name.as_ptr()), 1)
                .map_err(UserDefaultError::OpenProviderFailed)?;
            defer! {
                let _ = PTCloseProvider(provider);
            }
            let stream = SHCreateMemStream(None).ok_or(UserDefaultError::StreamNotAllocated)?;
            PTConvertDevModeToPrintTicket(
                provider,
                dev_mode_size,
                buffer.as_ptr() as *const DEVMODEA,
                kPTJobScope,
                &stream,
            )
            .map_err(UserDefaultError::ConvertDevModeToPrintTicketFailed)?;
            let xml = read_com_stream(&stream).map_err(UserDefaultError::ReadTicketFailed)?;
            Ok(Self { xml })
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
}

impl From<PrintTicketDocument> for PrintTicket {
    fn from(value: PrintTicketDocument) -> Self {
        Self {
            xml: value.to_xml(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PrintTicket;
    use crate::test_utils::null_device;

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
