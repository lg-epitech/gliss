// PID 1 of the opt-in Distribox-path benchmark guest (scripts/bench-distribox-path.mjs).
// It stands where a desktop session normally sits: a tiny compositor with a terminal,
// a draggable window and a video player, presented through virtio-gpu KMS and driven
// by the PS/2 keyboard and virtio tablet that QEMU's VNC server feeds.
//
// Keys: F1 static, F2 scrolling terminal, F3 window drag, F4 video, F5 reset. Letter
// keys are probes: each one types a cell into the terminal's input box.
// Marker strips (22 cells of 16x16 px, aligned to H.264 macroblocks so they survive
// lossy coding): white, black, 16 value bits (MSB first), 4 check bits.
//   K = probe key presses since the last reset
//   F = 60 Hz tick of the animation frame on screen
//   P = pointer x the dragged window was last drawn for
// Serial: "READY WxH" once; "T <tick> <late_us>" once per second while animating,
// where late_us is how long after the tick's nominal time the line was written;
// "S ..." statistics whenever a scene ends; "FAIL <reason>" on a fatal error.
#define _GNU_SOURCE
#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

enum { STATIC, SCROLL, DRAG, VIDEO, RESET };  // F1..F5
enum { CELL = 16, STRIP_CELLS = 22, STRIP_X = 16, K_Y = 16, F_Y = 48, P_Y = 80 };
enum { CHAR_W = 9, LINE_H = 18, GLYPH_W = 7, GLYPH_H = 12, GLYPHS = 64, INPUT_LINES = 4 };
enum { WINDOW_W = 600, WINDOW_H = 400, WINDOW_MIN_Y = 112, VIDEO_W = 640, VIDEO_H = 360 };
enum { SCROLL_STEP = 4, TICK_NS = 16666667, MAX_DAMAGE = 8 };
enum { PAPER = 0x16181d, INPUT_PAPER = 0x0c0d10, CURSOR = 0xe6e6e6 };

typedef struct { int x, y, w, h; } Rect;

static int card, keyboard, tablet, timer;
static uint32_t fb_id;
static int W, H, stride;
// Apps draw into `base`; `screen` is the scanout buffer and only ever receives
// composited damage, as a compositor's front buffer would.
static uint32_t *screen, *base, *window_pixels;
static Rect body, input, video, window;
static Rect damage[MAX_DAMAGE];
static int damages;

static int scene = STATIC, window_visible, button;
static int pointer_x, pointer_y, pending_x, pending_y;
static unsigned keys, cursor, pointer_drawn;
static unsigned shown_k = ~0u, shown_f = ~0u, shown_p = ~0u;
static long scroll;
static int64_t epoch;
static uint64_t tick, frame_shown, video_frame, reported_second;
static struct {
  unsigned frames, skipped, flushes, pointer;
  int64_t late_max, flush_sum, flush_max;
} stats;

static uint8_t glyphs[GLYPHS][GLYPH_H], wave[256], texture[256][256];

static void say(const char *format, ...) {
  char text[256];
  va_list arguments;
  va_start(arguments, format);
  int length = vsnprintf(text, sizeof(text), format, arguments);
  va_end(arguments);
  if (write(1, text, (size_t)length) < 0) {}
}

static void fail(const char *what) {
  say("FAIL %s: %s\n", what, strerror(errno));
  for (;;) pause();
}

static int64_t now_ns(void) {
  struct timespec time;
  clock_gettime(CLOCK_MONOTONIC, &time);
  return time.tv_sec * 1000000000LL + time.tv_nsec;
}

static uint32_t hash(uint32_t a, uint32_t b) {
  uint32_t h = a * 0x9e3779b1u ^ (b + 0x7f4a7c15u) * 0x85ebca6bu;
  h ^= h >> 15;
  h *= 0x2c1b3c6du;
  return h ^ h >> 13;
}

static int clamp(int value, int low, int high) { return value < low ? low : value > high ? high : value; }

// ---------------------------------------------------------------- drawing

static void add_damage(Rect r) {
  if (r.x < 0) r.w += r.x, r.x = 0;
  if (r.y < 0) r.h += r.y, r.y = 0;
  if (r.x + r.w > W) r.w = W - r.x;
  if (r.y + r.h > H) r.h = H - r.y;
  if (r.w <= 0 || r.h <= 0) return;
  if (damages < MAX_DAMAGE) {
    damage[damages++] = r;
    return;
  }
  Rect *last = &damage[MAX_DAMAGE - 1];
  int right = r.x + r.w > last->x + last->w ? r.x + r.w : last->x + last->w;
  int bottom = r.y + r.h > last->y + last->h ? r.y + r.h : last->y + last->h;
  if (r.x < last->x) last->x = r.x;
  if (r.y < last->y) last->y = r.y;
  last->w = right - last->x;
  last->h = bottom - last->y;
}

static void fill(Rect r, uint32_t color) {
  for (int y = r.y; y < r.y + r.h; y++)
    for (int x = r.x; x < r.x + r.w; x++) base[y * W + x] = color;
}

static void draw_glyph(int x, int y, int glyph, uint32_t ink) {
  for (int row = 0; row < GLYPH_H; row++)
    for (int bit = 0; bit < GLYPH_W; bit++)
      if (glyphs[glyph][row] >> bit & 1) base[(y + 3 + row) * W + x + 1 + bit] = ink;
}

// One scanline of an endless pseudo-document. Any row can be produced on demand, so
// scrolling renders only the rows that enter the viewport. Lines vary in length and
// colour like terminal output; glyphs are stroke patterns with the density of text.
static void text_row(uint32_t *out, int width, long y, uint32_t paper, uint32_t ink) {
  static const uint32_t inks[8] = {0xd4d4d4, 0xd4d4d4, 0xd4d4d4, 0xd4d4d4, 0x9cdc8a, 0xe5c07b, 0x7fb3e6, 0xe06c75};
  uint32_t line = (uint32_t)(y / LINE_H), seed = hash(line, 1);
  int row = (int)(y % LINE_H) - 3, columns = width / CHAR_W - 2;
  for (int x = 0; x < width; x++) out[x] = paper;
  if (row < 0 || row >= GLYPH_H || seed % 9 == 0) return;
  int length = 12 + (int)((seed >> 4) % (uint32_t)(columns - 12));
  if (!ink) ink = inks[seed >> 20 & 7];
  for (int column = 0; column < length; column++) {
    uint32_t h = hash(line, (uint32_t)column + 2);
    if (h % 6 == 0) continue;  // word gap
    uint8_t bits = glyphs[h >> 8 & (GLYPHS - 1)][row];
    uint32_t *cell = out + (column + 1) * CHAR_W + 1;
    for (int bit = 0; bit < GLYPH_W; bit++)
      if (bits >> bit & 1) cell[bit] = ink;
  }
}

static void draw_body(int from, int to) {
  for (int y = from; y < to; y++) text_row(base + (body.y + y) * W + body.x, body.w, scroll + y, PAPER, 0);
  add_damage(body);  // callers either repaint or move the whole viewport
}

static void scroll_body(int rows) {
  scroll += rows;
  if (rows < body.h)
    for (int y = 0; y < body.h - rows; y++)
      memcpy(base + (body.y + y) * W + body.x, base + (body.y + y + rows) * W + body.x, (size_t)body.w * 4);
  draw_body(rows < body.h ? body.h - rows : 0, body.h);
}

// Plasma over a panning noise texture: every pixel changes every frame, with both
// smooth gradients and fine detail, like camera footage.
static void draw_video(unsigned frame) {
  for (int y = 0; y < video.h; y++) {
    uint32_t *out = base + (video.y + y) * W + video.x;
    unsigned vertical = wave[(y * 2 - frame * 5) & 255];
    const uint8_t *grain = texture[(y + frame) & 255];
    for (int x = 0; x < video.w; x++) {
      unsigned v = wave[(x + frame * 3) & 255] + vertical + wave[((x + y) / 2 + frame * 7) & 255] +
                   grain[(x + frame * 2) & 255];
      out[x] = (uint32_t)wave[v & 255] << 16 | (uint32_t)wave[(v + 85) & 255] << 8 | wave[(v + 170) & 255];
    }
  }
  add_damage(video);
}

static Rect cursor_cell(unsigned index) {
  unsigned columns = (unsigned)(input.w - 8) / CHAR_W;
  return (Rect){input.x + 4 + (int)(index % columns) * CHAR_W, input.y + 4 + (int)(index / columns) * LINE_H,
                CHAR_W, LINE_H};
}

static void clear_input(void) {
  fill(input, INPUT_PAPER);
  fill(cursor_cell(0), CURSOR);
  add_damage(input);
  cursor = 0;
}

// Typing: the glyph replaces the block cursor, which moves one cell on. Two cells of
// damage, as in a real terminal.
static void type_glyph(int code) {
  unsigned capacity = (unsigned)(input.w - 8) / CHAR_W * INPUT_LINES;
  if (cursor + 1 >= capacity) clear_input();
  Rect cell = cursor_cell(cursor);
  fill(cell, INPUT_PAPER);
  draw_glyph(cell.x, cell.y, code % GLYPHS, 0xffffff);
  add_damage(cell);
  cell = cursor_cell(++cursor);
  fill(cell, CURSOR);
  add_damage(cell);
}

static void draw_marker(int y, unsigned value, unsigned *shown) {
  value &= 0xffff;
  if (*shown == value) return;
  *shown = value;
  // A reader that mistakes one cell fails the check instead of reporting progress.
  unsigned check = ((value ^ value >> 4 ^ value >> 8 ^ value >> 12) & 15) ^ 5;
  unsigned cells = 1u << (STRIP_CELLS - 1) | value << 4 | check;
  for (int n = 0; n < STRIP_CELLS; n++)
    fill((Rect){STRIP_X + n * CELL, y, CELL, CELL}, cells >> (STRIP_CELLS - 1 - n) & 1 ? 0xffffff : 0);
  add_damage((Rect){STRIP_X, y, STRIP_CELLS * CELL, CELL});
}

static void draw_desktop(void) {
  static const uint32_t icons[5] = {0xe0703c, 0x4c9be8, 0x58b368, 0xd9b03f, 0xa56cc1};
  for (int y = 0; y < H; y++)
    for (int x = 0; x < W; x++)
      base[y * W + x] = (uint32_t)(28 + 36 * y / H) << 16 | (uint32_t)(44 + 50 * y / H + 20 * x / W) << 8 |
                        (uint32_t)(84 + 60 * x / W);
  fill((Rect){0, 0, STRIP_X * 2 + STRIP_CELLS * CELL, P_Y + CELL + 16}, 0x202020);
  for (int n = 0; n < 5; n++) fill((Rect){48, 176 + n * 96, 56, 56}, icons[n]);
  fill((Rect){body.x - 2, body.y - 30, body.w + 4, body.h + input.h + 42}, 0x3c4048);
  fill((Rect){body.x - 2, body.y - 30, body.w + 4, 28}, 0x2a2d33);
  for (int n = 0; n < 3; n++) fill((Rect){body.x + 10 + n * 22, body.y - 22, 12, 12}, icons[n]);
  draw_body(0, body.h);
  clear_input();
  add_damage((Rect){0, 0, W, H});
}

static void make_assets(void) {
  uint32_t state = 0x2545f491;
#define RANDOM() (state ^= state << 13, state ^= state >> 17, state ^= state << 5, state)
  for (int glyph = 0; glyph < GLYPHS; glyph++)
    for (int strokes = 2 + (int)(RANDOM() % 3); strokes > 0; strokes--) {
      if (RANDOM() & 1) {
        int from = (int)(RANDOM() % 4), to = from + 2 + (int)(RANDOM() % (uint32_t)(GLYPH_W - from - 2));
        int row = (int)(RANDOM() % GLYPH_H);
        for (int bit = from; bit <= to; bit++) glyphs[glyph][row] |= (uint8_t)(1 << bit);
      } else {
        int from = (int)(RANDOM() % 6), to = from + 3 + (int)(RANDOM() % (uint32_t)(GLYPH_H - from - 3));
        int bit = (int)(RANDOM() % GLYPH_W);
        for (int row = from; row <= to; row++) glyphs[glyph][row] |= (uint8_t)(1 << bit);
      }
    }
  for (int n = 0; n < 256; n++) {  // smooth 0..255 hump, a sine without libm
    int t = n < 128 ? n : 255 - n;
    wave[n] = (uint8_t)(t * t * (381 - 2 * t) * 255 / 2048383);
  }
  for (int y = 0; y < 256; y++)
    for (int x = 0; x < 256; x++) texture[y][x] = RANDOM() & 15;  // mild film grain
#undef RANDOM
  window_pixels = malloc((size_t)WINDOW_W * WINDOW_H * 4);
  for (int y = 0; y < WINDOW_H; y++) {
    uint32_t *row = window_pixels + y * WINDOW_W;
    for (int x = 0; x < WINDOW_W; x++) row[x] = y < 28 ? 0x3b6ea5 : 0xf4f4f4;
    if (y >= 40 && y < WINDOW_H - 12) text_row(row + 12, WINDOW_W - 24, 90000 + y, 0xf4f4f4, 0x202428);
    if (y == 0 || y == WINDOW_H - 1) for (int x = 0; x < WINDOW_W; x++) row[x] = 0x1c1c1c;
    row[0] = row[WINDOW_W - 1] = 0x1c1c1c;
  }
}

// ---------------------------------------------------------------- presenting

static void composite(Rect r) {
  for (int y = r.y; y < r.y + r.h; y++)
    memcpy(screen + (size_t)y * stride + r.x, base + y * W + r.x, (size_t)r.w * 4);
  if (!window_visible) return;
  int left = r.x > window.x ? r.x : window.x, top = r.y > window.y ? r.y : window.y;
  int right = r.x + r.w < window.x + window.w ? r.x + r.w : window.x + window.w;
  int bottom = r.y + r.h < window.y + window.h ? r.y + r.h : window.y + window.h;
  for (int y = top; y < bottom && left < right; y++)
    memcpy(screen + (size_t)y * stride + left, window_pixels + (y - window.y) * WINDOW_W + left - window.x,
           (size_t)(right - left) * 4);
}

// Everything one wakeup changed goes out in a single DIRTYFB, so a key press and the
// animation frame it lands on never wait for each other.
// What Linux 7.1's virtio-gpu does with it, as seen from the host:
// - The upload and flush reach QEMU when the commit starts, but the ioctl then sleeps
//   until the driver's simulated vblank (75 Hz in QEMU's preferred mode). Input that
//   arrives meanwhile is drawn up to one vblank late; `late_max_us` reports it.
// - The clips are ignored: the first framebuffer change sets ignore_damage_clips for
//   good, so every flush covers the whole plane and QEMU's VNC server finds the
//   changed tiles by comparing frames. Older guest kernels flush only the clips.
static void flush(void) {
  draw_marker(K_Y, keys, &shown_k);
  draw_marker(F_Y, (unsigned)frame_shown, &shown_f);
  draw_marker(P_Y, pointer_drawn, &shown_p);
  if (!damages) return;
  struct drm_clip_rect clips[MAX_DAMAGE];
  for (int n = 0; n < damages; n++) {
    composite(damage[n]);
    clips[n] = (struct drm_clip_rect){(uint16_t)damage[n].x, (uint16_t)damage[n].y,
                                      (uint16_t)(damage[n].x + damage[n].w), (uint16_t)(damage[n].y + damage[n].h)};
  }
  struct drm_mode_fb_dirty_cmd dirty = {.fb_id = fb_id, .clips_ptr = (uintptr_t)clips, .num_clips = (uint32_t)damages};
  int64_t started = now_ns();
  if (ioctl(card, DRM_IOCTL_MODE_DIRTYFB, &dirty)) fail("DIRTYFB");
  int64_t took = now_ns() - started;
  damages = 0;
  stats.flushes++;
  stats.flush_sum += took;
  if (took > stats.flush_max) stats.flush_max = took;
  if ((scene == SCROLL || scene == VIDEO) && frame_shown / 60 != reported_second) {
    reported_second = frame_shown / 60;
    say("T %llu %lld\n", (unsigned long long)frame_shown,
        (long long)((now_ns() - epoch - (int64_t)frame_shown * TICK_NS) / 1000));
  }
}

// ---------------------------------------------------------------- scenes and input

static void place_window(void) {
  window = (Rect){clamp(pointer_x - WINDOW_W / 2, 0, W - WINDOW_W),
                  clamp(pointer_y - 14, WINDOW_MIN_Y, H - WINDOW_H), WINDOW_W, WINDOW_H};
}

// Animation frames are numbered by an absolute 60 Hz clock that starts with the
// program, so the host can date any frame it sees from the F marker alone.
static void arm_timer(int enabled) {
  struct itimerspec spec = {0};
  if (enabled) {
    tick = (uint64_t)((now_ns() - epoch) / TICK_NS);
    int64_t first = epoch + (int64_t)(tick + 1) * TICK_NS;
    spec.it_value = (struct timespec){first / 1000000000, first % 1000000000};
    spec.it_interval.tv_nsec = TICK_NS;
  }
  if (timerfd_settime(timer, TFD_TIMER_ABSTIME, &spec, NULL)) fail("timerfd_settime");
}

static void set_scene(int next) {
  say("S scene=%d frames=%u skipped=%u late_max_us=%lld flushes=%u flush_avg_us=%lld flush_max_us=%lld keys=%u "
      "pointer=%u\n", scene, stats.frames, stats.skipped, (long long)(stats.late_max / 1000), stats.flushes,
      (long long)(stats.flushes ? stats.flush_sum / stats.flushes / 1000 : 0), (long long)(stats.flush_max / 1000),
      keys, stats.pointer);
  memset(&stats, 0, sizeof(stats));
  if (scene == DRAG) {
    window_visible = 0;
    add_damage(window);
  }
  if (next == RESET) {
    keys = 0;
    scroll = 0;
    pointer_drawn = 0;
    clear_input();
  }
  if (scene == VIDEO || next == RESET) draw_body(0, body.h);
  scene = next == RESET ? STATIC : next;
  if (scene == DRAG) {
    place_window();
    window_visible = 1;
    add_damage(window);
  }
  arm_timer(scene == SCROLL || scene == VIDEO);
}

static void read_keyboard(void) {
  struct input_event events[64];
  for (ssize_t size; (size = read(keyboard, events, sizeof(events))) > 0;)
    for (size_t n = 0; n < (size_t)size / sizeof(events[0]); n++) {
      int code = events[n].code;
      if (events[n].type != EV_KEY || events[n].value != 1) continue;  // presses only, no autorepeat
      if (code >= KEY_F1 && code <= KEY_F5) {
        set_scene(code - KEY_F1);
      } else if ((code >= KEY_Q && code <= KEY_P) || (code >= KEY_A && code <= KEY_L) ||
                 (code >= KEY_Z && code <= KEY_M)) {
        keys++;
        type_glyph(code);
      }
    }
}

static void read_tablet(void) {
  struct input_event events[64];
  int moved = 0;
  for (ssize_t size; (size = read(tablet, events, sizeof(events))) > 0;)
    for (size_t n = 0; n < (size_t)size / sizeof(events[0]); n++) {
      if (events[n].type == EV_ABS && events[n].code == ABS_X) pending_x = events[n].value;
      if (events[n].type == EV_ABS && events[n].code == ABS_Y) pending_y = events[n].value;
      if (events[n].type == EV_KEY && events[n].code == BTN_LEFT) button = events[n].value;
      if (events[n].type != EV_SYN) continue;
      // QEMU scales VNC pixels to 0..32767 rounding down; rounding to nearest here
      // recovers the exact pixel the client sent.
      pointer_x = clamp((int)(((int64_t)pending_x * W + 16383) / 32767), 0, W - 1);
      pointer_y = clamp((int)(((int64_t)pending_y * H + 16383) / 32767), 0, H - 1);
      stats.pointer++;
      moved = 1;
    }
  // Several queued reports collapse into one redraw at the newest position.
  if (!moved || scene != DRAG || !button) return;
  add_damage(window);
  place_window();
  add_damage(window);
  pointer_drawn = (unsigned)pointer_x;
}

static void on_timer(void) {
  uint64_t expirations;
  if (read(timer, &expirations, sizeof(expirations)) != sizeof(expirations)) return;
  tick += expirations;  // a late wakeup skips frames instead of shifting the clock
  stats.skipped += (unsigned)(expirations - 1);
  int64_t late = now_ns() - epoch - (int64_t)tick * TICK_NS;
  if (late > stats.late_max) stats.late_max = late;
  if (scene == SCROLL) {
    scroll_body((int)(SCROLL_STEP * expirations));
  } else if (scene == VIDEO && tick / 2 != video_frame) {  // 30 Hz content
    video_frame = tick / 2;
    draw_video((unsigned)video_frame);
  } else {
    return;
  }
  frame_shown = tick;
  stats.frames++;
}

// ---------------------------------------------------------------- setup

static void load_modules(void) {
  FILE *list = fopen("/modules", "r");
  char path[256];
  if (!list) fail("/modules");
  while (fgets(path, sizeof(path), list)) {
    path[strcspn(path, "\n")] = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0 || syscall(SYS_finit_module, fd, "", 0)) say("MODULE %s: %s\n", path, strerror(errno));
    close(fd);
  }
  fclose(list);
}

// The firmware framebuffer may hold a card until virtio-gpu replaces it, so the
// card is chosen by driver name rather than by number.
static int open_card(void) {
  for (int attempt = 0; attempt < 500; attempt++, usleep(20000))
    for (int n = 0; n < 4; n++) {
      char path[32], name[32] = "";
      snprintf(path, sizeof(path), "/dev/dri/card%d", n);
      int fd = open(path, O_RDWR | O_CLOEXEC);
      if (fd < 0) continue;
      struct drm_version version = {.name = name, .name_len = sizeof(name) - 1};
      if (!ioctl(fd, DRM_IOCTL_VERSION, &version) && !strcmp(name, "virtio_gpu")) return fd;
      close(fd);
    }
  errno = ENODEV;
  fail("virtio-gpu card");
  return -1;
}

static int open_input(int want_tablet) {
  for (int attempt = 0; attempt < 500; attempt++, usleep(20000))
    for (int n = 0; n < 16; n++) {
      char path[32];
      uint8_t absolute[ABS_MAX / 8 + 1] = {0}, key[KEY_MAX / 8 + 1] = {0};
      snprintf(path, sizeof(path), "/dev/input/event%d", n);
      int fd = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
      if (fd < 0) continue;
      ioctl(fd, EVIOCGBIT(EV_ABS, sizeof(absolute)), absolute);
      ioctl(fd, EVIOCGBIT(EV_KEY, sizeof(key)), key);
      int is_tablet = absolute[ABS_X / 8] >> (ABS_X % 8) & 1, is_keyboard = key[KEY_A / 8] >> (KEY_A % 8) & 1;
      if (want_tablet ? is_tablet : is_keyboard && !is_tablet) return fd;
      close(fd);
    }
  errno = ENODEV;
  fail(want_tablet ? "virtio tablet" : "AT keyboard");
  return -1;
}

// Raw KMS: the connector's preferred mode is the size QEMU was started with.
static void open_display(void) {
  uint32_t connectors[8], crtcs[8];
  static struct drm_mode_modeinfo modes[256];
  struct drm_mode_modeinfo *mode = NULL;
  card = open_card();
  ioctl(card, DRM_IOCTL_SET_MASTER, 0);
  struct drm_mode_card_res resources = {.connector_id_ptr = (uintptr_t)connectors, .count_connectors = 8,
                                        .crtc_id_ptr = (uintptr_t)crtcs, .count_crtcs = 8};
  if (ioctl(card, DRM_IOCTL_MODE_GETRESOURCES, &resources) || !resources.count_connectors || !resources.count_crtcs)
    fail("GETRESOURCES");
  struct drm_mode_get_connector probe = {.connector_id = connectors[0]};  // no buffers: forces a probe
  if (ioctl(card, DRM_IOCTL_MODE_GETCONNECTOR, &probe)) fail("GETCONNECTOR probe");
  // The kernel copies modes only when the whole list fits.
  struct drm_mode_get_connector connector = {.connector_id = connectors[0], .modes_ptr = (uintptr_t)modes,
                                             .count_modes = 256};
  errno = E2BIG;
  if (!probe.count_modes || probe.count_modes > 256 || ioctl(card, DRM_IOCTL_MODE_GETCONNECTOR, &connector) ||
      connector.count_modes != probe.count_modes)
    fail("GETCONNECTOR modes");
  for (uint32_t n = 0; n < connector.count_modes; n++)
    if (!mode || (modes[n].type & DRM_MODE_TYPE_PREFERRED && !(mode->type & DRM_MODE_TYPE_PREFERRED))) mode = &modes[n];
  W = mode->hdisplay;
  H = mode->vdisplay;
  say("MODE %s %u Hz, %u modes\n", mode->name, mode->vrefresh, connector.count_modes);
  struct drm_mode_create_dumb dumb = {.width = (uint32_t)W, .height = (uint32_t)H, .bpp = 32};
  if (ioctl(card, DRM_IOCTL_MODE_CREATE_DUMB, &dumb)) fail("CREATE_DUMB");
  struct drm_mode_fb_cmd framebuffer = {.width = (uint32_t)W, .height = (uint32_t)H, .pitch = dumb.pitch, .bpp = 32,
                                        .depth = 24, .handle = dumb.handle};  // XRGB8888
  if (ioctl(card, DRM_IOCTL_MODE_ADDFB, &framebuffer)) fail("ADDFB");
  struct drm_mode_map_dumb map = {.handle = dumb.handle};
  if (ioctl(card, DRM_IOCTL_MODE_MAP_DUMB, &map)) fail("MAP_DUMB");
  screen = mmap(NULL, dumb.size, PROT_READ | PROT_WRITE, MAP_SHARED, card, (off_t)map.offset);
  if (screen == MAP_FAILED) fail("mmap");
  fb_id = framebuffer.fb_id;
  stride = (int)(dumb.pitch / 4);
  struct drm_mode_crtc crtc = {.crtc_id = crtcs[0], .fb_id = fb_id, .set_connectors_ptr = (uintptr_t)connectors,
                               .count_connectors = 1, .mode = *mode, .mode_valid = 1};
  if (ioctl(card, DRM_IOCTL_MODE_SETCRTC, &crtc)) fail("SETCRTC");
}

int main(void) {
  mkdir("/dev", 0755);
  mkdir("/proc", 0755);
  mkdir("/sys", 0755);
  mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
  mount("proc", "/proc", "proc", 0, NULL);
  mount("sysfs", "/sys", "sysfs", 0, NULL);
  load_modules();
  open_display();
  keyboard = open_input(0);
  tablet = open_input(1);
  timer = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
  if (timer < 0) fail("timerfd_create");

  body.x = 448;
  body.y = 96;
  body.w = clamp(W - body.x - 32, 0, 1280) / 16 * 16;
  body.h = clamp(H - body.y - INPUT_LINES * LINE_H - 52, 0, 720) / 16 * 16;
  input = (Rect){body.x, body.y + body.h + 8, body.w, INPUT_LINES * LINE_H + 8};
  video = (Rect){body.x + (body.w - VIDEO_W) / 2, body.y + (body.h - VIDEO_H) / 2, VIDEO_W, VIDEO_H};
  if (body.w < VIDEO_W || body.h < VIDEO_H || H < WINDOW_MIN_Y + WINDOW_H) {
    errno = EINVAL;
    fail("display too small");
  }
  base = malloc((size_t)W * H * 4);
  pointer_x = W / 2;
  pointer_y = H / 2;
  make_assets();
  draw_desktop();
  epoch = now_ns();
  flush();
  say("READY %dx%d\n", W, H);

  struct pollfd sources[3] = {{keyboard, POLLIN, 0}, {tablet, POLLIN, 0}, {timer, POLLIN, 0}};
  for (;;) {
    if (poll(sources, 3, -1) < 0 && errno != EINTR) fail("poll");
    if (sources[0].revents) read_keyboard();
    if (sources[1].revents) read_tablet();
    if (sources[2].revents) on_timer();
    flush();
  }
}
