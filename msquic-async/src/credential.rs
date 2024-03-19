use std::boxed::Box;
use std::ffi::CString;
use std::marker::PhantomPinned;
use std::path::Path;
use std::pin::Pin;
use std::ptr;

pub trait CredentialConfig {
    fn as_cred_config_ref<'a>(self: &'a Pin<&Self>) -> &'a msquic::CredentialConfig;
    fn set_flags(self: &mut Pin<&mut Self>, flags: msquic::CredentialFlags);
}

#[repr(C)]
pub struct CredentialConfigCertFile {
    pub cred_config: msquic::CredentialConfig,
    cert_file: msquic::CertificateFile,
    cert_file_path: CString,
    key_file_path: CString,
    _pinned: PhantomPinned,
}

impl CredentialConfigCertFile {
    pub fn new(cert_file_path: &Path, key_file_path: &Path) -> Pin<Box<Self>> {
        let mut boxed = Box::into_pin(Box::new(CredentialConfigCertFile {
            cred_config: msquic::CredentialConfig {
                cred_type: msquic::CREDENTIAL_TYPE_CERTIFICATE_FILE,
                cred_flags: msquic::CREDENTIAL_FLAG_NONE,
                certificate: msquic::CertificateUnion { file: ptr::null() },
                principle: ptr::null(),
                reserved: ptr::null(),
                async_handler: None,
                allowed_cipher_suites: 0,
            },
            cert_file: msquic::CertificateFile {
                private_key_file: ptr::null(),
                certificate_file: ptr::null(),
            },
            cert_file_path: CString::new(cert_file_path.to_str().unwrap().as_bytes()).unwrap(),
            key_file_path: CString::new(key_file_path.to_str().unwrap().as_bytes()).unwrap(),
            _pinned: PhantomPinned,
        }));

        let certificate_file_ptr = boxed.cert_file_path.as_ptr() as _;
        let private_key_file_ptr = boxed.key_file_path.as_ptr() as _;

        let cert_file_ptr = &boxed.cert_file as _;
        unsafe {
            Pin::get_unchecked_mut(Pin::as_mut(&mut boxed))
                .cert_file
                .certificate_file = certificate_file_ptr;
            Pin::get_unchecked_mut(Pin::as_mut(&mut boxed))
                .cert_file
                .private_key_file = private_key_file_ptr;
            Pin::get_unchecked_mut(Pin::as_mut(&mut boxed))
                .cred_config
                .certificate
                .file = cert_file_ptr;
        }
        boxed
    }
}

impl CredentialConfig for CredentialConfigCertFile {
    fn as_cred_config_ref<'a, 'b>(self: &'a Pin<&'b Self>) -> &'b msquic::CredentialConfig {
        &self.get_ref().cred_config
    }

    fn set_flags(self: &mut Pin<&mut Self>, flags: msquic::CredentialFlags) {
        unsafe {
            Pin::get_unchecked_mut(self.as_mut()).cred_config.cred_flags = flags;
        }
    }
}