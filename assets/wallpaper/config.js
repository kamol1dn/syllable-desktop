// Starting defaults. Normally the look is changed in the app's settings, which are
// pushed to the page over the feed and remembered by it.
// Any key can also be given in the URL: index.html?background=dynamic&lyricssize=120
window.WALLPAPER_CONFIG = {
  // "wallpapers" = random image from `folder` on every song change, "dynamic" = album gradient
  background: "wallpapers",
  // Listed by the app (which must be running).
  folder: "",            // e.g. "C:\\Users\\you\\Pictures\\Wallpapers"
  // Filter by subfolder or filename prefix (nord_..., gruvbox_...): all, calm, digital, gruvbox, monochrome, nord, others
  collection: "all",
  wallpaperblur: 0,      // px
  kenburns: true,        // slow zoom drift on wallpapers
  dim: 35,               // darken background, %
  dynspeed: 100,         // gradient speed, %
  audioreactive: true,   // gradient pulses with the bass

  // "split" (cover + lyrics), "mirror" (lyrics + cover), "lyrics" (lyrics only),
  // "karaoke" (one big centred line), "corner" (small panel bottom-right), "lock" (centred)
  layout: "split",
  lyricssize: 100,       // %
  linepos: 45,           // active line height, % of the screen
  lineblur: true,        // blur lines away from the current one
  lyricstyle: "glide",   // glide | punch | mist
  offset: 0,             // ms, positive = lyrics earlier
  clock: true,           // clock when nothing is playing
  fps: 60,
  port: "9012",          // the app's local feed (the host passes ?port= anyway)

  // lockscreen.html applies these on top of everything above.
  lockscreen: {
    layout: "lock",      // lyrics centred under Windows' own clock + a small now-playing strip
    clock: false,        // Windows draws the time on the lock screen already
    dim: 45,
  },
};
