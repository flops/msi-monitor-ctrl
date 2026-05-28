#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crossbeam_channel::{Receiver, Sender};
use tracing::{Level, event};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
  GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
  CallNextHookEx, DispatchMessageW, GetMessageW, MSG, MSLLHOOKSTRUCT, SetWindowsHookExW,
  TranslateMessage, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
  WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::{MouseButton, MouseHotkey};

struct Shared {
  hotkeys: Mutex<HashMap<MouseButton, Vec<MouseHotkey>>>,
  pending: Mutex<HashMap<MouseButton, MouseHotkey>>,
  tx: Sender<MouseHotkey>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

pub fn init() -> Receiver<MouseHotkey> {
  let (tx, rx) = crossbeam_channel::unbounded();
  let _ = SHARED.set(Shared {
    hotkeys: Mutex::new(HashMap::new()),
    pending: Mutex::new(HashMap::new()),
    tx,
  });
  std::thread::Builder::new()
    .name("mouse-hook".into())
    .spawn(run_hook_thread)
    .expect("spawn mouse-hook thread");
  rx
}

pub fn register(hk: MouseHotkey) {
  if let Some(shared) = SHARED.get() {
    let mut hks = shared.hotkeys.lock().unwrap();
    let list = hks.entry(hk.button).or_default();
    if !list.contains(&hk) {
      list.push(hk);
    }
  }
}

fn run_hook_thread() {
  unsafe {
    let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), None, 0) {
      Ok(h) => h,
      Err(e) => {
        event!(Level::ERROR, error = %e, "failed to install low-level mouse hook");
        return;
      },
    };

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
      let _ = TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }

    let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook);
  }
}

unsafe extern "system" fn low_level_mouse_proc(
  code: i32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if code < 0 {
    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
  }

  let Some(shared) = SHARED.get() else {
    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
  };

  let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
  let msg = wparam.0 as u32;

  let (button, is_down) = match msg {
    WM_LBUTTONDOWN => (Some(MouseButton::Left), true),
    WM_LBUTTONUP => (Some(MouseButton::Left), false),
    WM_RBUTTONDOWN => (Some(MouseButton::Right), true),
    WM_RBUTTONUP => (Some(MouseButton::Right), false),
    WM_MBUTTONDOWN => (Some(MouseButton::Middle), true),
    WM_MBUTTONUP => (Some(MouseButton::Middle), false),
    WM_XBUTTONDOWN | WM_XBUTTONUP => {
      let which = (info.mouseData >> 16) as u16;
      let btn = if which == XBUTTON1 {
        Some(MouseButton::Back)
      } else if which == XBUTTON2 {
        Some(MouseButton::Forward)
      } else {
        None
      };
      (btn, msg == WM_XBUTTONDOWN)
    },
    _ => (None, false),
  };

  let Some(button) = button else {
    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
  };

  if is_down {
    let ctrl = is_pressed(VK_CONTROL.0 as i32);
    let shift = is_pressed(VK_SHIFT.0 as i32);
    let alt = is_pressed(VK_MENU.0 as i32);
    let meta = is_pressed(VK_LWIN.0 as i32) || is_pressed(VK_RWIN.0 as i32);

    let matched = {
      let hks = shared.hotkeys.lock().unwrap();
      hks.get(&button).and_then(|list| {
        list
          .iter()
          .copied()
          .find(|hk| hk.ctrl == ctrl && hk.shift == shift && hk.alt == alt && hk.meta == meta)
      })
    };

    if let Some(hk) = matched {
      shared.pending.lock().unwrap().insert(button, hk);
      return LRESULT(1);
    }
  } else if let Some(hk) = shared.pending.lock().unwrap().remove(&button) {
    let _ = shared.tx.send(hk);
    return LRESULT(1);
  }

  unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn is_pressed(vk: i32) -> bool {
  unsafe { (GetAsyncKeyState(vk) as u32 & 0x8000) != 0 }
}
