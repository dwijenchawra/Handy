//! CoreAudio output mute without spawning an AppleScript interpreter.
use std::ffi::c_void;
use std::sync::Mutex;

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectSetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
}

static PREVIOUS: Mutex<Option<(u32, u32)>> = Mutex::new(None);

fn read(object: u32, address: &Address) -> Result<u32, String> {
    let mut value = 0_u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: CoreAudio receives valid pointers to a u32 and its byte size.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            address,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    if status != 0 {
        return Err(format!("CoreAudio property read failed: {status}"));
    }
    Ok(value)
}

pub fn set_muted(muted: bool) -> Result<(), String> {
    let mut previous = PREVIOUS.lock().map_err(|e| e.to_string())?;
    let address = Address {
        selector: u32::from_be_bytes(*b"mute"),
        scope: u32::from_be_bytes(*b"outp"),
        element: 0,
    };
    let (device, value) = if muted {
        if previous.is_some() {
            return Ok(());
        }
        let device = read(
            1,
            &Address {
                selector: u32::from_be_bytes(*b"dOut"),
                scope: u32::from_be_bytes(*b"glob"),
                element: 0,
            },
        )?;
        let value = read(device, &address)?;
        *previous = Some((device, value));
        (device, 1_u32)
    } else {
        match *previous {
            Some(saved) => saved,
            None => return Ok(()),
        }
    };
    // SAFETY: the property expects a u32, provided for the duration of the call.
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            std::mem::size_of::<u32>() as u32,
            (&value as *const u32).cast(),
        )
    };
    if status != 0 {
        if muted {
            *previous = None;
        }
        return Err(format!("CoreAudio mute failed: {status}"));
    }
    if !muted {
        *previous = None;
    }
    Ok(())
}
