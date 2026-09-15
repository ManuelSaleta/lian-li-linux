# Pixel conditioning

Each LCD card has a pixel-conditioning control with 15, 30, 60, and 120-minute presets. The daemon generates a five-second pattern of alternating black/white, changing grayscale noise, and solid color phases. JPEG-only displays use native-sized frames within their payload limits. H.264 displays use noise generated at reduced resolution and upscaled to the panel's native dimensions before encoding; bitrate and frame bursts follow the negotiated block size, or the driver's fallback when negotiation is unavailable. There is no bundled video or download. Preparation finishes before the display changes; use Cancel to abandon preparation or Stop to restore the previous display.

```bash
lianli-daemon lcd clean --minutes 30
lianli-daemon lcd clean --device-id 'hid:1-2:1.0#0' --minutes 15
```

Omitting `--device-id` selects active configured LCD targets. The `#0` suffix identifies the configuration entry, not the physical USB port. Ctrl+C or SIGTERM cancels preparation or stops the session started by that CLI process. CLI durations must be positive; values above 255 minutes are capped.

Conditioning uses 75% brightness and restores the configured brightness afterward (100% when omitted). Config reload cancels conditioning before applying new entries. On daemon shutdown, supported LCD backlights are turned off by default. Disable **Turn off LCDs on shutdown** in Settings to skip this brightness change. Startup reapplies configured brightness. This routine exercises pixels; it does not guarantee recovery from retention or panel damage.
