use std::{collections::HashSet, fs::File, io::Write as _, os::fd::OwnedFd};

use log::{debug, error, trace};
use xkbcommon::xkb::{
    CONTEXT_NO_FLAGS, Context, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1, KeyDirection,
    Keycode, Keymap, KeymapFormat, LayoutIndex, LayoutMask, LevelIndex, ModMask,
    STATE_LAYOUT_DEPRESSED, STATE_LAYOUT_EFFECTIVE, STATE_LAYOUT_LATCHED, STATE_LAYOUT_LOCKED,
    STATE_MODS_DEPRESSED, STATE_MODS_LATCHED, STATE_MODS_LOCKED, State,
};
use xkeysym::Keysym;

use crate::{InputError, InputResult, Key};

mod parse_keymap;
pub(crate) use parse_keymap::ParsedKeymap;
mod default_keymap;
use default_keymap::DEFAULT_KEYMAP;

pub struct Keymap2 {
    original_keymap: String,
    context: Context,
    keymap: Keymap,
    state: State,
    parsed_keymap: ParsedKeymap,
    pressed_keys: HashSet<Keycode>,
}

// This is safe, we have a unique pointer.
unsafe impl Send for Keymap2 {}

impl Keymap2 {
    pub(crate) fn new_from_fd(
        context: Context,
        format: KeymapFormat,
        fd: OwnedFd,
        size: u32,
    ) -> Result<Self, ()> {
        use std::io::{Read, Seek, SeekFrom};

        // Bound keymap parse cost / DoS from a buggy or malicious compositor.
        const MAX_KEYMAP_SIZE: u32 = 4 * 1024 * 1024; // 4 MiB

        debug!("creating new xkb:Keymap");
        debug!("new(format: {format}, size: {size}, ...)");

        if size == 0 || size > MAX_KEYMAP_SIZE {
            error!("keymap size {size} is outside 1..={MAX_KEYMAP_SIZE}; resetting the keymap");
            return Err(());
        }

        let mut keymap_file = File::from(fd);

        // Check if the file size is correct
        let metadata = keymap_file.metadata().map_err(|e| {
            error!("could not get the file's metadata! Skipping file size check. Error: {e}");
        })?;
        if metadata.len() != size.into() {
            error!("file does not have the expected size! resetting the keymap");
            return Err(());
        }

        // Read keymap to String
        let mut original_keymap = String::new();
        // Reset the cursor to the beginning of the file.
        keymap_file.seek(SeekFrom::Start(0)).map_err(|e| {
            error!("unable to seek from the start:\n{e}");
        })?;
        keymap_file
            .read_to_string(&mut original_keymap)
            .map_err(|e| {
                error!("unable to read file to string:\n{e}");
            })?;
        // The String cannot end with NULL otherwise xkbcommon will fail to parse it
        while original_keymap.ends_with('\0') {
            debug!("removed NULL byte at the end");
            original_keymap.pop();
        }
        trace!("keymap string getting parsed by xkbcommon:\n{original_keymap}");

        let keymap = Keymap::new_from_string(
            &context,
            original_keymap.clone(),
            format,
            KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            error!("unable to parse the keymap with xkbcommon! resetting the keymap");
        })?;

        let state = State::new(&keymap);

        Self::new(context, original_keymap, keymap, state)
    }

    fn new(
        context: Context,
        mut original_keymap: String,
        keymap: Keymap,
        state: State,
    ) -> Result<Self, ()> {
        // The String cannot end with NULL otherwise xkbcommon will fail to parse it
        while original_keymap.ends_with('\0') {
            debug!("removed NULL byte at the end");
            original_keymap.pop();
        }
        trace!("keymap string getting parsed by xkbcommon:\n{original_keymap}");

        let parsed_keymap = ParsedKeymap::try_from(original_keymap.as_str()).map_err(|()| {
            trace!("unable to parse the new keymap");
        })?;

        Ok(Self {
            original_keymap,
            context,
            keymap,
            state,
            parsed_keymap,
            pressed_keys: HashSet::with_capacity(8),
        })
    }

    pub fn update(&mut self, format: KeymapFormat, fd: OwnedFd, size: u32) -> Result<(), ()> {
        let depressed_mods = self.state.serialize_mods(STATE_MODS_DEPRESSED);
        let latched_mods = self.state.serialize_mods(STATE_MODS_LATCHED);
        let locked_mods = self.state.serialize_mods(STATE_MODS_LOCKED);

        let depressed_layout = self.state.serialize_layout(STATE_LAYOUT_DEPRESSED);
        let latched_layout = self.state.serialize_layout(STATE_LAYOUT_LATCHED);
        let locked_layout = self.state.serialize_layout(STATE_LAYOUT_LOCKED);

        let Keymap2 {
            context,
            keymap,
            mut state,
            parsed_keymap,
            pressed_keys,
            original_keymap: _, // Never update the original keymap
        } = Self::new_from_fd(self.context.clone(), format, fd, size).map_err(|()| {
            trace!("unable to create new keymap");
        })?;

        // The docs say this is a bad idea. update_key and update_mask should not get
        // mixed. I don't know how else to get the same state though
        for key in pressed_keys {
            state.update_key(key, KeyDirection::Down);
        }

        state.update_mask(
            depressed_mods,
            latched_mods,
            locked_mods,
            depressed_layout,
            latched_layout,
            locked_layout,
        );

        self.context = context;
        self.keymap = keymap;
        self.state = state;
        self.parsed_keymap = parsed_keymap;

        Ok(())
    }

    /// Update the state and return the new bitflags for the modifiers and the
    /// effective layout if they changed. If they remained the same, None is
    /// returned
    pub fn update_key(
        &mut self,
        keycode: Keycode,
        direction: KeyDirection,
    ) -> Option<(ModMask, ModMask, ModMask, LayoutMask)> {
        let depressed_mods_old = self.state.serialize_mods(STATE_MODS_DEPRESSED);
        let latched_mods_old = self.state.serialize_mods(STATE_MODS_LATCHED);
        let locked_mods_old = self.state.serialize_mods(STATE_MODS_LOCKED);
        let effective_layout_old = self.state.serialize_layout(STATE_LAYOUT_EFFECTIVE);

        match direction {
            KeyDirection::Up => {
                self.pressed_keys.remove(&keycode);
            }
            KeyDirection::Down => {
                self.pressed_keys.insert(keycode);
            }
        }
        self.state.update_key(keycode, direction);

        let depressed_mods_new = self.state.serialize_mods(STATE_MODS_DEPRESSED);
        let latched_mods_new = self.state.serialize_mods(STATE_MODS_LATCHED);
        let locked_mods_new = self.state.serialize_mods(STATE_MODS_LOCKED);
        let effective_layout_new = self.state.serialize_layout(STATE_LAYOUT_EFFECTIVE);

        if depressed_mods_old != depressed_mods_new
            || latched_mods_old != latched_mods_new
            || locked_mods_old != locked_mods_new
            || effective_layout_old != effective_layout_new
        {
            Some((
                depressed_mods_new,
                latched_mods_new,
                locked_mods_new,
                effective_layout_new,
            ))
        } else {
            None
        }
    }

    pub fn update_modifiers(
        &mut self,
        depressed_mods: ModMask,
        latched_mods: ModMask,
        locked_mods: ModMask,
        depressed_layout: LayoutIndex,
        latched_layout: LayoutIndex,
        locked_layout: LayoutIndex,
    ) {
        self.state.update_mask(
            depressed_mods,
            latched_mods,
            locked_mods,
            depressed_layout,
            latched_layout,
            locked_layout,
        );
    }

    pub fn format_file_size(&self) -> Result<(KeymapFormat, File, u32), ()> {
        let mut keymap_file = tempfile::tempfile().map_err(|e| {
            error!("could not create temporary file. Error: {e}");
        })?;
        // Compositors (e.g. wlroots) mmap `size` bytes and pass the mapping to
        // xkb_keymap_new_from_string(), which requires a NUL within that range.
        write!(keymap_file, "{}\0", self.parsed_keymap).map_err(|e| {
            error!("could not write to temporary file. Error: {e}");
        })?;
        let metadata = keymap_file.metadata().map_err(|e| {
            error!("could not get the file's metadata! Error: {e}");
        })?;
        let size = metadata.len().try_into().map_err(|_| {
            error!(
                "keymap file is {} but the maximum is {} (u32::MAX)",
                metadata.len(),
                u32::MAX
            );
        })?;

        let format = KEYMAP_FORMAT_TEXT_V1;

        Ok((format, keymap_file, size))
    }

    /// Find a key on the current keymap that produces `key`, together with the
    /// modifiers needed to reach the shift level it sits on.
    ///
    /// Searching level by level rather than keycode by keycode is what makes
    /// the result the *cheapest* way to type the keysym: a key that needs no
    /// modifier always wins over one that does, whatever order the keycodes
    /// happen to fall in.
    ///
    /// Looking at level 0 alone — which is all `xkb_state_key_get_one_sym`
    /// reports while no modifier is held — misses every shifted character. On a
    /// US layout that is all of `A`-`Z` and `! " # % & * + : > ? @ ^ _ { | } ~`,
    /// none of which appear unshifted on any key. Each miss then fell through
    /// to [`Self::map_key`], which appends a keycode to the keymap and makes the
    /// caller upload the whole thing mid-sentence; a standard keymap has no free
    /// keycode below 256, so the invented one starts at 257 and is dropped
    /// outright by X11 clients (keycodes are a `CARD8` there) and raced against
    /// the keymap upload by Wayland ones. Either way the character silently
    /// vanished.
    pub fn key_to_keycode(&self, key: Key) -> Option<(u16, ModMask)> {
        let keysym = Keysym::from(key);
        let min = self.keymap.min_keycode().raw();
        let max = self.keymap.max_keycode().raw();

        // Keys do not all have the same number of levels, so the loop runs
        // until no key has a level this deep rather than up to a fixed depth.
        for level in 0.. {
            let mut level_exists = false;

            for raw in min..=max {
                let keycode = Keycode::new(raw);
                // The active group only: reaching a keysym by switching layout
                // would leave the user on a different one than they started on.
                let layout = self.state.key_get_layout(keycode);
                let syms = self.keymap.key_get_syms_by_level(keycode, layout, level);
                if syms.is_empty() {
                    continue;
                }
                level_exists = true;

                // One keysym exactly. A level that emits several would type the
                // others alongside the one asked for.
                if syms != [keysym] {
                    continue;
                }
                let Ok(keycode_u16) = u16::try_from(raw) else {
                    continue;
                };
                let Some(mods) = self.mods_for_level(keycode, layout, level) else {
                    // No modifier combination reaches this level, so the key is
                    // there but unreachable. Keep looking.
                    continue;
                };
                trace!("keysym {keysym:?} is on keycode {raw} at level {level} (mods {mods:#x})");
                return Some((keycode_u16, mods));
            }

            if !level_exists {
                return None;
            }
        }

        None
    }

    /// The fewest modifiers that select `level` on this key, if any do.
    fn mods_for_level(
        &self,
        keycode: Keycode,
        layout: LayoutIndex,
        level: LevelIndex,
    ) -> Option<ModMask> {
        // Four is room for every combination a sane layout offers; a key that
        // needs more simply loses the alternatives that did not fit, and the
        // ones kept are still correct.
        let mut masks = [ModMask::default(); 4];
        let found = self
            .keymap
            .key_get_mods_for_level(keycode, layout, level, &mut masks);
        masks[..found]
            .iter()
            .copied()
            .min_by_key(|mask| mask.count_ones())
    }

    /// The modifier and layout state as the compositor should see it, with
    /// `extra` merged into the depressed modifiers.
    ///
    /// Passing `0` restores what is actually held.
    pub fn modifier_state(&self, extra: ModMask) -> (ModMask, ModMask, ModMask, LayoutMask) {
        (
            self.state.serialize_mods(STATE_MODS_DEPRESSED) | extra,
            self.state.serialize_mods(STATE_MODS_LATCHED),
            self.state.serialize_mods(STATE_MODS_LOCKED),
            self.state.serialize_layout(STATE_LAYOUT_EFFECTIVE),
        )
    }

    /// The keys that hold down `mods`: for each modifier in it, the lowest
    /// keycode whose press sets exactly that modifier, as `Shift_L` sets Shift.
    ///
    /// `None` when some modifier in `mods` has no such key on this keymap.
    pub fn modifier_keycodes(&self, mods: ModMask) -> Option<Vec<u16>> {
        let min = self.keymap.min_keycode().raw();
        let max = self.keymap.max_keycode().raw();
        (0..ModMask::BITS)
            .map(|index| 1 << index)
            .filter(|bit| mods & bit != 0)
            .map(|bit| {
                let raw = (min..=max).find(|&raw| {
                    let keycode = Keycode::new(raw);
                    // A key held for the caller is not ours to press and release.
                    if self.pressed_keys.contains(&keycode) {
                        return false;
                    }
                    // A fresh state each time: a lock or latch key would leave
                    // a shared one changed.
                    let mut state = State::new(&self.keymap);
                    state.update_key(keycode, KeyDirection::Down);
                    state.serialize_mods(STATE_MODS_DEPRESSED) == bit
                })?;
                u16::try_from(raw).ok()
            })
            .collect()
    }

    pub fn map_key(&mut self, key: Key) -> InputResult<u16> {
        let keysym = Keysym::from(key);
        let key_name = format!("{keysym:?}");
        let key_name = match key_name.strip_prefix("XK_") {
            Some(key_name) => key_name,
            None => &key_name,
        };
        // A held key must keep its identity until it is released, so it is
        // neither handed out as free nor stolen.
        let held: HashSet<u32> = self.pressed_keys.iter().map(|k| k.raw()).collect();
        let keycode = self.parsed_keymap.map_key(key_name, &held)?;
        self.recompile().map_err(|()| {
            InputError::Mapping("unable to compile the updated keymap".to_string())
        })?;
        Ok(keycode)
    }

    fn recompile(&mut self) -> Result<(), ()> {
        let depressed_mods = self.state.serialize_mods(STATE_MODS_DEPRESSED);
        let latched_mods = self.state.serialize_mods(STATE_MODS_LATCHED);
        let locked_mods = self.state.serialize_mods(STATE_MODS_LOCKED);

        let depressed_layout = self.state.serialize_layout(STATE_LAYOUT_DEPRESSED);
        let latched_layout = self.state.serialize_layout(STATE_LAYOUT_LATCHED);
        let locked_layout = self.state.serialize_layout(STATE_LAYOUT_LOCKED);

        let keymap = Keymap::new_from_string(
            &self.context,
            self.parsed_keymap.to_string(),
            KEYMAP_FORMAT_TEXT_V1,
            KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            error!("unable to compile the updated keymap with xkbcommon");
        })?;

        let mut state = State::new(&keymap);
        for key in &self.pressed_keys {
            state.update_key(*key, KeyDirection::Down);
        }
        state.update_mask(
            depressed_mods,
            latched_mods,
            locked_mods,
            depressed_layout,
            latched_layout,
            locked_layout,
        );

        self.keymap = keymap;
        self.state = state;
        Ok(())
    }

    pub fn unmap_everything(&mut self) -> InputResult<()> {
        debug!("unmap_everything");
        let depressed_mods = self.state.serialize_mods(STATE_MODS_DEPRESSED);
        let latched_mods = self.state.serialize_mods(STATE_MODS_LATCHED);
        let locked_mods = self.state.serialize_mods(STATE_MODS_LOCKED);

        let depressed_layout = self.state.serialize_layout(STATE_LAYOUT_DEPRESSED);
        let latched_layout = self.state.serialize_layout(STATE_LAYOUT_LATCHED);
        let locked_layout = self.state.serialize_layout(STATE_LAYOUT_LOCKED);

        let mut keymap_file = tempfile::tempfile().map_err(|e| {
            error!("could not create temporary file. Error: {e}");
            InputError::Unmapping("unable to unmap the keys".to_string())
        })?;
        write!(keymap_file, "{}", self.original_keymap).map_err(|e| {
            error!("could not write to temporary file. Error: {e}");
            InputError::Unmapping("unable to unmap the keys".to_string())
        })?;
        let metadata = keymap_file.metadata().map_err(|e| {
            error!("could not get the file's metadata! Error: {e}");
            InputError::Unmapping("unable to unmap the keys".to_string())
        })?;
        let size = metadata.len().try_into().map_err(|_| {
            error!(
                "keymap file is {} but the maximum is {} (u32::MAX)",
                metadata.len(),
                u32::MAX
            );
            InputError::Unmapping("unable to unmap the keys".to_string())
        })?;

        let Keymap2 {
            context,
            keymap,
            mut state,
            mut parsed_keymap,
            pressed_keys: _,    // We don't change the mapping of pressed keys
            original_keymap: _, // Never update the original keymap
        } = Self::new_from_fd(
            self.context.clone(),
            KEYMAP_FORMAT_TEXT_V1,
            keymap_file.into(),
            size,
        )
        .map_err(|()| {
            trace!("unable to create new keymap");
            InputError::Unmapping("unable to unmap the keys".to_string())
        })?;

        state.update_mask(
            depressed_mods,
            latched_mods,
            locked_mods,
            depressed_layout,
            latched_layout,
            locked_layout,
        );

        // Copy the current mapping for all pressed keys so they can get released
        let pressed_keys = self.pressed_keys.iter().map(|k| u32::from(*k)).collect();
        parsed_keymap.copy_maps_for_keycodes(&self.parsed_keymap, &pressed_keys);

        self.context = context;
        self.keymap = keymap;
        self.state = state;
        self.parsed_keymap = parsed_keymap;

        Ok(())
    }

    pub fn default() -> Result<Self, ()> {
        debug!("Default keymap is used");
        let mut keymap_file = tempfile::tempfile().map_err(|e| {
            error!("could not create temporary file. Error: {e}");
        })?;
        write!(keymap_file, "{DEFAULT_KEYMAP}").map_err(|e| {
            error!("could not write DEFAULT KEYMAP to temporary file. Error: {e}");
        })?;
        let metadata = keymap_file.metadata().map_err(|e| {
            error!("could not get the file's metadata! Error: {e}");
        })?;
        let size = metadata.len().try_into().map_err(|_| {
            error!(
                "keymap file is {} but the maximum is {} (u32::MAX)",
                metadata.len(),
                u32::MAX
            );
        })?;

        let format = KEYMAP_FORMAT_TEXT_V1;
        let context = Context::new(CONTEXT_NO_FLAGS);

        Self::new_from_fd(context, format, keymap_file.into(), size)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use xkbcommon::xkb::{
        CONTEXT_NO_FLAGS, Context, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1, Keymap,
        MOD_NAME_SHIFT,
    };

    use super::Keymap2;
    use crate::Key;

    fn us_keymap() -> Keymap2 {
        let context = Context::new(CONTEXT_NO_FLAGS);
        let keymap =
            Keymap::new_from_names(&context, "", "", "us", "", None, KEYMAP_COMPILE_NO_FLAGS)
                .expect("compiling a us keymap requires the xkb data files");
        let keymap_string = keymap.get_as_string(KEYMAP_FORMAT_TEXT_V1);
        let mut file = tempfile::tempfile().unwrap();
        write!(file, "{keymap_string}\0").unwrap();
        let size = file.metadata().unwrap().len().try_into().unwrap();
        Keymap2::new_from_fd(context, KEYMAP_FORMAT_TEXT_V1, file.into(), size).unwrap()
    }

    // After a key is dynamically mapped, the compiled keymap must reflect the
    // mapping immediately, without waiting for the compositor to echo the
    // uploaded keymap back. The echoes lag behind the uploads, so relying on
    // them lets lookups and later mappings operate on stale state.
    #[test]
    fn unit_mapped_key_is_found_without_compositor_echo() {
        let mut keymap = us_keymap();

        let key = Key::Unicode('汉');
        assert_eq!(keymap.key_to_keycode(key), None, "precondition: not mapped");
        let keycode = keymap.map_key(key).unwrap();
        assert_eq!(keymap.key_to_keycode(key).map(|(k, _)| k), Some(keycode));

        // A second mapping must not displace the first one.
        let key2 = Key::Unicode('语');
        let keycode2 = keymap.map_key(key2).unwrap();
        assert_ne!(keycode, keycode2);
        assert_eq!(keymap.key_to_keycode(key).map(|(k, _)| k), Some(keycode));
        assert_eq!(keymap.key_to_keycode(key2).map(|(k, _)| k), Some(keycode2));
    }

    // The bug this pins: `I` lives only on shift level 1 of the `i` key, so a
    // lookup that reads level 0 alone reports it as absent. The caller then
    // invents a keycode for it — one above 255, which X11 clients drop and
    // Wayland clients race against the keymap upload — and the character
    // silently disappears from the typed text. Uppercase letters and `?`/`!`
    // must resolve to a key the layout already has, plus Shift.
    #[test]
    fn unit_shifted_characters_use_the_unshifted_key_plus_shift() {
        let keymap = us_keymap();

        let (lower, lower_mods) = keymap
            .key_to_keycode(Key::Unicode('i'))
            .expect("`i` is on the us layout");
        assert_eq!(lower_mods, 0, "`i` needs no modifier");

        // Shift specifically, not Caps: both reach level 1 of an alphabetic
        // key, and `key_get_mods_for_level` offers both, but Caps is a locked
        // modifier and sending it depressed is not how a keyboard types a
        // capital.
        let shift = 1 << keymap.keymap.mod_get_index(MOD_NAME_SHIFT);

        for c in ['I', 'Z', '?', '!', ':', '"'] {
            let (keycode, mods) = keymap
                .key_to_keycode(Key::Unicode(c))
                .unwrap_or_else(|| panic!("`{c}` is on the us layout, at a shift level"));
            assert_eq!(mods, shift, "`{c}` must be typed with Shift held");
            assert!(
                keycode <= 255,
                "`{c}` resolved to keycode {keycode}; above 255 is unrepresentable in X11"
            );
        }

        // Same physical key as its lowercase twin, so no keymap upload is
        // needed to type it.
        let (upper, _) = keymap.key_to_keycode(Key::Unicode('I')).unwrap();
        assert_eq!(upper, lower, "`I` is Shift plus the `i` key");
    }

    // Off-layout characters get a key the keymap already names, never one
    // invented above 255: X11 carries keycodes in a byte, so XWayland drops
    // anything larger and the character silently never arrives. Typed into
    // xterm, `I think I am OK at NASA` landed as ` think  am  at ` for exactly
    // this reason. The pool is finite; when it and the stealable keys are gone
    // the error has to say so, and unmapping has to start it over.
    #[test]
    fn unit_off_layout_characters_get_keycodes_x11_can_carry() {
        let mut keymap = us_keymap();
        // More distinct CJK characters than there are keycodes under 256, so
        // the pool — free keys, then stealable ones — must run dry.
        let chars: Vec<char> = ('\u{4e00}'..).take(300).collect();

        let mut mapped = Vec::new();
        let mut exhausted = None;
        for &c in &chars {
            match keymap.map_key(Key::Unicode(c)) {
                Ok(keycode) => {
                    assert!(keycode <= 255, "{c} got keycode {keycode}, above 255");
                    assert!(!mapped.contains(&keycode), "{c} reused keycode {keycode}");
                    assert_eq!(
                        keymap.key_to_keycode(Key::Unicode(c)),
                        Some((keycode, 0)),
                        "{c} must be found where it was mapped, with no modifier"
                    );
                    mapped.push(keycode);
                }
                Err(e) => {
                    exhausted = Some(e.to_string());
                    break;
                }
            }
        }
        assert!(
            mapped.len() >= 14,
            "a stock us layout leaves at least 14 keys free, got {}",
            mapped.len()
        );
        let err = exhausted.expect("300 characters must exhaust the pool");
        assert!(
            err.contains("255"),
            "the error must name the ceiling: {err}"
        );

        // Unmapping returns the pool, and the next mapping is under 255 again.
        keymap.unmap_everything().unwrap();
        let again = keymap.map_key(Key::Unicode('\u{4e00}')).unwrap();
        assert!(again <= 255);
    }

    // CI canary for xkeyboard-config. Every layout it ships leaves the ten
    // reserved `<I1xx>` evdev keys unbound (their only mention in the symbols
    // data is commented out), which is what makes a fresh keymap always have
    // room. If an upstream release binds them, this fails here instead of in
    // someone's editor.
    #[test]
    fn unit_stock_layout_leaves_the_reserved_keys_free() {
        let keymap = us_keymap();
        let free = keymap.parsed_keymap.free_key_names();
        assert!(
            free.len() >= 10,
            "only {} free keys under 256: {free:?}",
            free.len()
        );
        for reserved in [
            "I120", "I149", "I154", "I168", "I178", "I183", "I184", "I222", "I230", "I248",
        ] {
            assert!(
                free.iter().any(|n| n == reserved),
                "<{reserved}> is bound on a stock us layout; free: {free:?}"
            );
        }
    }

    // The keys pressed around a shifted character. Shift must come out as
    // `Shift_L`, a key every keyboard has, and a modifier the keymap has no
    // key for must say so rather than press something else.
    #[test]
    fn unit_level_modifiers_are_found_on_their_keys() {
        let keymap = us_keymap();
        let shift = 1 << keymap.keymap.mod_get_index(MOD_NAME_SHIFT);
        let shift_l = keymap.keymap.key_by_name("LFSH").unwrap().raw();

        assert_eq!(
            keymap.modifier_keycodes(shift),
            Some(vec![u16::try_from(shift_l).unwrap()]),
            "Shift is held with `Shift_L`"
        );
        assert_eq!(keymap.modifier_keycodes(0), Some(vec![]), "nothing to hold");
        assert_eq!(
            keymap.modifier_keycodes(1 << 31),
            None,
            "a modifier the keymap does not define has no key"
        );
    }

    // Level 0 has to win. Searching keycode by keycode instead of level by
    // level would let a key that needs Shift shadow an unmodified one that sits
    // later in the keymap, so plain letters would start typing with Shift held.
    #[test]
    fn unit_unmodified_keys_win_over_shifted_ones() {
        let keymap = us_keymap();

        for c in ['a', 'z', '1', '9', '/', ';', '-', '=', ','] {
            let (_, mods) = keymap
                .key_to_keycode(Key::Unicode(c))
                .unwrap_or_else(|| panic!("`{c}` is on the us layout"));
            assert_eq!(mods, 0, "`{c}` is unshifted and must need no modifier");
        }
    }
}
