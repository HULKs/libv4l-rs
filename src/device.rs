use std::convert::TryFrom;
use std::path::Path;
use std::sync::Arc;
use std::{io, mem};

use crate::control;
use crate::v4l2;
use crate::v4l_sys::*;
use crate::{capability::Capabilities, control::Control};
use thiserror::Error;

pub enum OpenFlags {
    Nonblocking = 0,
    Blocking = 1,
}

#[derive(Error, Debug)]
pub enum WaitError {
    #[error("poll did not return an image before timeout")]
    Timeout,
    #[error("poll returned -1. errno: {0}")]
    PollError(errno::Errno),
    #[error("polled event was not POLLIN. revents: {0}")]
    DeviceError(i16),
}

/// Linux capture device abstraction
pub struct Device {
    /// Raw handle
    handle: Arc<Handle>,
}

impl Device {
    /// Returns a capture device by index
    ///
    /// Devices are usually enumerated by the system.
    /// An index of zero thus represents the first device the system got to know about.
    ///
    /// # Arguments
    ///
    /// * `index` - Index (0: first, 1: second, ..)
    ///
    /// # Example
    ///
    /// ```
    /// use v4l::device::Device;
    /// let dev = Device::new(0);
    /// ```
    pub fn new(index: usize) -> io::Result<Self> {
        let path = format!("{}{}", "/dev/video", index);
        let fd = v4l2::open(&path, libc::O_RDWR)?;

        if fd == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Device {
            handle: Arc::new(Handle { fd }),
        })
    }

    /// Returns a capture device by path
    ///
    /// Linux device nodes are usually found in /dev/videoX or /sys/class/video4linux/videoX.
    ///
    /// # Arguments
    ///
    /// * `path` - Path (e.g. "/dev/video0")
    ///
    /// # Example
    ///
    /// ```
    /// use v4l::device::Device;
    /// let dev = Device::with_path("/dev/video0");
    /// ```
    pub fn with_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let fd = v4l2::open(&path, libc::O_RDWR)?;

        if fd == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Device {
            handle: Arc::new(Handle { fd }),
        })
    }

    /// Returns a capture device by path and specified flags
    ///
    /// Linux device nodes are usually found in /dev/videoX or /sys/class/video4linux/videoX.
    ///
    /// # Arguments
    ///
    /// * `path` - Path (e.g. "/dev/video0")
    /// * `flags` - i32 (e.g. libc::O_RDWR)
    ///
    /// # Example
    ///
    /// ```
    /// use v4l::device::Device;
    /// use libc;
    /// let dev = Device::with_path("/dev/video0", libc::O_RDWR | libc::O_NONBLOCK);
    /// ```
    pub fn with_path_and_flags<P: AsRef<Path>>(path: P, open_flags: OpenFlags) -> io::Result<Self> {
        let flags = libc::O_RDWR
            | match open_flags {
                OpenFlags::Nonblocking => libc::O_NONBLOCK,
                OpenFlags::Blocking => 0,
            };
        let fd = v4l2::open(&path, flags)?;

        if fd == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Device {
            handle: Arc::new(Handle { fd }),
        })
    }

    /// Returns the raw device handle
    pub fn handle(&self) -> Arc<Handle> {
        self.handle.clone()
    }

    /// Returns video4linux framework defined information such as card, driver, etc.
    pub fn query_caps(&self) -> io::Result<Capabilities> {
        unsafe {
            let mut v4l2_caps: v4l2_capability = mem::zeroed();
            v4l2::ioctl(
                self.handle().fd(),
                v4l2::vidioc::VIDIOC_QUERYCAP,
                &mut v4l2_caps as *mut _ as *mut std::os::raw::c_void,
            )?;

            Ok(Capabilities::from(v4l2_caps))
        }
    }

    /// Returns the supported controls for a device such as gain, focus, white balance, etc.
    pub fn query_controls(&self) -> io::Result<Vec<control::Description>> {
        let mut controls = Vec::new();
        unsafe {
            let mut v4l2_ctrl: v4l2_queryctrl = mem::zeroed();

            loop {
                v4l2_ctrl.id |= V4L2_CTRL_FLAG_NEXT_CTRL;
                v4l2_ctrl.id |= V4L2_CTRL_FLAG_NEXT_COMPOUND;
                match v4l2::ioctl(
                    self.handle().fd(),
                    v4l2::vidioc::VIDIOC_QUERYCTRL,
                    &mut v4l2_ctrl as *mut _ as *mut std::os::raw::c_void,
                ) {
                    Ok(_) => {
                        // get the basic control information
                        let mut control = control::Description::from(v4l2_ctrl);

                        // if this is a menu control, enumerate its items
                        if control.typ == control::Type::Menu
                            || control.typ == control::Type::IntegerMenu
                        {
                            let mut items = Vec::new();

                            let mut v4l2_menu: v4l2_querymenu = mem::zeroed();
                            v4l2_menu.id = v4l2_ctrl.id;

                            for i in (v4l2_ctrl.minimum..=v4l2_ctrl.maximum)
                                .step_by(v4l2_ctrl.step as usize)
                            {
                                v4l2_menu.index = i as u32;
                                let res = v4l2::ioctl(
                                    self.handle().fd(),
                                    v4l2::vidioc::VIDIOC_QUERYMENU,
                                    &mut v4l2_menu as *mut _ as *mut std::os::raw::c_void,
                                );

                                // BEWARE OF DRAGONS!
                                // The API docs [1] state VIDIOC_QUERYMENU should may return EINVAL
                                // for some indices between minimum and maximum when an item is not
                                // supported by a driver.
                                //
                                // I have no idea why it is advertised in the first place then, but
                                // have seen this happen with a Logitech C920 HD Pro webcam.
                                // In case of errors, let's just skip the offending index.
                                //
                                // [1] https://github.com/torvalds/linux/blob/master/Documentation/userspace-api/media/v4l/vidioc-queryctrl.rst#description
                                if res.is_err() {
                                    continue;
                                }

                                let item =
                                    control::MenuItem::try_from((control.typ, v4l2_menu)).unwrap();
                                items.push((v4l2_menu.index, item));
                            }

                            control.items = Some(items);
                        }

                        controls.push(control);
                    }
                    Err(e) => {
                        if controls.is_empty() || e.kind() != io::ErrorKind::InvalidInput {
                            return Err(e);
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        Ok(controls)
    }

    /// Returns the control value for an ID
    ///
    /// # Arguments
    ///
    /// * `id` - Control identifier
    pub fn control(&self, id: u32) -> io::Result<Control> {
        unsafe {
            let mut v4l2_ctrl: v4l2_control = mem::zeroed();
            v4l2_ctrl.id = id;
            v4l2::ioctl(
                self.handle().fd(),
                v4l2::vidioc::VIDIOC_G_CTRL,
                &mut v4l2_ctrl as *mut _ as *mut std::os::raw::c_void,
            )?;

            Ok(Control::Value(v4l2_ctrl.value))
        }
    }

    /// Modifies the control value
    ///
    /// # Arguments
    ///
    /// * `id` - Control identifier
    /// * `val` - New value
    pub fn set_control(&self, id: u32, val: Control) -> io::Result<()> {
        unsafe {
            let mut v4l2_ctrl: v4l2_control = mem::zeroed();
            v4l2_ctrl.id = id;
            match val {
                Control::Value(val) => v4l2_ctrl.value = val,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "only single value controls are supported at the moment",
                    ))
                }
            }
            v4l2::ioctl(
                self.handle().fd(),
                v4l2::vidioc::VIDIOC_S_CTRL,
                &mut v4l2_ctrl as *mut _ as *mut std::os::raw::c_void,
            )
        }
    }

    pub fn wait(&self, timeout: Option<usize>) -> Result<(), WaitError> {
        let mut file_descriptors = [libc::pollfd {
            fd: self.handle().fd(),
            events: libc::POLLIN | libc::POLLPRI,
            revents: 0,
        }];
        let timeout = match timeout {
            Some(t) => t as i32,
            None => -1,
        };
        let number_of_events = unsafe { libc::poll(file_descriptors.as_mut_ptr(), 1, timeout) };
        match number_of_events {
            -1 => Err(WaitError::PollError(errno::errno())),
            0 => Err(WaitError::Timeout),
            _ => {
                if file_descriptors[0].revents & libc::POLLIN != 0 {
                    Ok(())
                } else {
                    Err(WaitError::DeviceError(file_descriptors[0].revents))
                }
            }
        }
    }

    pub fn close(&mut self) {
        drop(self.handle());
    }
}

impl io::Read for Device {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let ret = libc::read(
                self.handle().fd(),
                buf.as_mut_ptr() as *mut std::os::raw::c_void,
                buf.len(),
            );
            match ret {
                -1 => Err(io::Error::last_os_error()),
                ret => Ok(ret as usize),
            }
        }
    }
}

impl io::Write for Device {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let ret = libc::write(
                self.handle().fd(),
                buf.as_ptr() as *const std::os::raw::c_void,
                buf.len(),
            );

            match ret {
                -1 => Err(io::Error::last_os_error()),
                ret => Ok(ret as usize),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // write doesn't use a buffer, so it effectively flushes with each call
        // therefore, we don't have anything to flush later
        Ok(())
    }
}

/// Device handle for low-level access.
///
/// Acquiring a handle facilitates (possibly mutating) interactions with the device.
pub struct Handle {
    fd: std::os::raw::c_int,
}

impl Handle {
    /// Returns the raw file descriptor
    pub fn fd(&self) -> std::os::raw::c_int {
        self.fd
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.fd >= 0 {
            v4l2::close(self.fd).unwrap();
            self.fd = -1;
        }
    }
}
