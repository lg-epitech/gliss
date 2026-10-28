#include <alsa/asoundlib.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <termios.h>
#include <stdint.h>
#include <inttypes.h>
#include <linux/fb.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <stdatomic.h>

static _Atomic unsigned int *moving;

static uint64_t nanos(void) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  return (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
}

// A procedural contention workload, not a window manager or a real application.
static uint32_t pixel(unsigned int x, unsigned int y, unsigned int frame) {
  uint32_t color = 0x18202b;
  unsigned int wx = 40 + (frame * 7) % 430, wy = 70 + (frame * 3) % 210;
  if (x >= wx && x < wx + 420 && y >= wy && y < wy + 260) {
    color = y < wy + 24 ? 0x3074b5 : 0xe0e4e8;
    if (y >= wy + 40 && ((y - wy) % 18 < 3) && x < wx + 80 + ((y / 18) * 53) % 300)
      color = 0x293c50;
  }
  if (x >= 570 && x < 990 && y >= 64 && y < 500) {
    unsigned int row = (y + frame * 4) / 16;
    color = row % 2 ? 0xe5ebed : 0xd8e0e5;
    if ((y + frame * 4) % 16 < 3 && x < 590 + (row * 73) % 380) color = 0x203848;
  }
  // Fixed 360x180 animated texture stresses video bandwidth like a changing
  // picture region. It is deliberately more varied than the document shapes.
  if (x >= 600 && x < 960 && y >= 540 && y < 720) {
    uint32_t hash = x * 2654435761U ^ y * 2246822519U ^ frame * 3266489917U;
    hash ^= hash >> 15; hash *= 2246822519U; hash ^= hash >> 13;
    color = hash & 0xffffff;
  }
  // 24-bit source frame counter plus an eight-bit XOR check, in large cells.
  // Source counters describe this marker only, not whole-frame coherence.
  if (y >= 8 && y < 28) {
    if (x >= 8 && x < 28) color = 0xf010f0;
    if (x >= 360 && x < 380) color = 0x10f0f0;
    if (x >= 32 && x < 352) {
      unsigned int bit = (x - 32) / 10;
      uint32_t check = ((frame & 255) ^ ((frame >> 8) & 255) ^ ((frame >> 16) & 255) ^ 0x5a) << 24;
      color = ((frame | check) >> bit) & 1 ? 0xe8e8e8 : 0x181818;
    }
  }
  return color;
}

static void display(void) {
  int fb = open("/dev/fb0", O_RDWR);
  struct fb_var_screeninfo var;
  struct fb_fix_screeninfo fix;
  if (fb < 0 || ioctl(fb, FBIOGET_VSCREENINFO, &var)) goto fail;
  var.xres = var.xres_virtual = 1024;var.yres = var.yres_virtual = 768;
  var.xoffset = var.yoffset = 0;var.bits_per_pixel = 32;
  if (ioctl(fb, FBIOPUT_VSCREENINFO, &var) || ioctl(fb, FBIOGET_VSCREENINFO, &var) || ioctl(fb, FBIOGET_FSCREENINFO, &fix)) goto fail;
  if (var.xres != 1024 || var.yres != 768 || var.bits_per_pixel != 32 ||
      var.red.offset != 16 || var.green.offset != 8 || var.blue.offset != 0 ||
      var.red.length != 8 || var.green.length != 8 || var.blue.length != 8 ||
      fix.line_length < 4096 || fix.line_length > 8192 ||
      fix.smem_len < fix.line_length * 768 || fix.smem_len > 32 * 1024 * 1024) goto fail;
  unsigned char *screen = mmap(NULL, fix.smem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fb, 0);
  if (screen == MAP_FAILED) goto fail;
  unsigned int frame = 0;
  uint64_t deadline = nanos(), started = deadline;
  int first = 1, geometry_reported = 0;
  for (;;) {
    if (!geometry_reported && nanos() - started >= 1000000000ULL) {
      struct fb_var_screeninfo current;
      if (!ioctl(fb, FBIOGET_VSCREENINFO, &current))
        dprintf(STDOUT_FILENO, "GLISS_AUDIO_MOTION_GEOMETRY %u %u %u\n", current.xres, current.yres, current.bits_per_pixel);
      geometry_reported = 1;
    }
    if (first || atomic_load(moving)) {
      if (!first) frame++;
      for (unsigned int y = 0; y < 768; y++) {
        uint32_t *row = (uint32_t *)(screen + y * fix.line_length);
        for (unsigned int x = 0; x < 1024; x++) row[x] = pixel(x, y, frame);
      }
      dprintf(STDOUT_FILENO, "GLISS_AUDIO_MOTION_FRAME %u %" PRIu64 "\n", frame, nanos());
      if (first) dprintf(STDOUT_FILENO, "GLISS_AUDIO_MOTION_READY 1024 768 32 30Hz\n");
      first = 0;
    }
    deadline += 1000000000ULL / 30;
    uint64_t now = nanos();
    if (deadline < now) deadline = now + 1000000000ULL / 30;
    struct timespec target = {.tv_sec = deadline / 1000000000ULL, .tv_nsec = deadline % 1000000000ULL};
    while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &target, NULL) == EINTR) {}
  }
fail:
  dprintf(STDOUT_FILENO, "GLISS_AUDIO_MOTION_ERROR framebuffer %s\n", strerror(errno));
  _exit(1);
}

// The serial control is a test stimulus, not a guest keyboard or audio clock.
static uint32_t last_token = 0;
static int current_code = 0, remaining = 0;
static char command[32];
static size_t used = 0;

static void commands(int fd, unsigned long sample) {
  char byte;
  uint32_t newest = 0;
  int code = 0;
  while (read(fd, &byte, 1) == 1) {
    if (used < sizeof(command) - 1) command[used++] = byte;
    if (byte != '\n') continue;
    if (used == 3 && command[0] == 'M' && (command[1] == '0' || command[1] == '1')) {
      unsigned int enabled = command[1] - '0';
      atomic_store(moving, enabled);
      printf("GLISS_AUDIO_MOTION_MODE %u %lu\n", enabled, sample);fflush(stdout);
    }
    if (used == 13 && command[0] == 'P' && command[9] == ':') {
      uint32_t value = 0;
      int valid = 1;
      for (int i = 1; i < 9; i++) {
        char c = command[i];
        int digit = c >= '0' && c <= '9' ? c - '0' :
                    c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
        if (digit < 0) { valid = 0; break; }
        value = (value << 4) | (uint32_t)digit;
      }
      int selected = 0;
      for (int i = 10; i < 12; i++) {
        char c = command[i];
        int digit = c >= '0' && c <= '9' ? c - '0' :
                    c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
        if (digit < 0) { valid = 0; break; }
        selected = (selected << 4) | digit;
      }
      if (valid && selected >= 1 && selected <= 48 && value > last_token) {
        last_token = value;
        newest = value;
        code = selected;
      }
    }
    used = 0;
  }
  if (newest) {
    current_code = code;
    remaining = 8 * 960;
    printf("GLISS_AUDIO_MARKER %" PRIu32 " %d %lu\n", newest, code, sample);
    fflush(stdout);
  }
}

int main(void) {
  if (getpid() != 1) {fputs("Guest init must run as PID 1\n", stderr);return 65;}
  mkdir("/dev", 0755);
  mkdir("/proc", 0755);
  mkdir("/sys", 0755);
  mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
  mount("proc", "/proc", "proc", 0, NULL);
  mount("sysfs", "/sys", "sysfs", 0, NULL);
  FILE *mods = fopen("/modules", "r");
  char path[512];
  while (fgets(path, sizeof(path), mods)) {
    path[strcspn(path, "\n")] = 0;
    int fd = open(path, O_RDONLY);
    int err = syscall(SYS_finit_module, fd, "", 0);
    if (err)
      printf("MODULE %s: %s\n", path, strerror(errno));
    close(fd);
  }
  fclose(mods);
  moving = mmap(NULL, sizeof(*moving), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (moving == MAP_FAILED) return 1;
  atomic_init(moving, 0);
  pid_t renderer = fork();
  if (renderer < 0) return 1;
  if (renderer == 0) display();
  sleep(1);
  setenv("ALSA_CONFIG_PATH", "/alsa.conf", 1);
  snd_ctl_t *ctl = NULL;
  if (!snd_ctl_open(&ctl, "test", 0)) {
    snd_ctl_elem_list_t *list;
    snd_ctl_elem_list_alloca(&list);
    snd_ctl_elem_list(ctl, list);
    unsigned int count = snd_ctl_elem_list_get_count(list);
    snd_ctl_elem_list_alloc_space(list, count);
    snd_ctl_elem_list(ctl, list);
    for (unsigned int i = 0; i < count; i++) {
      snd_ctl_elem_id_t *id;
      snd_ctl_elem_id_alloca(&id);
      snd_ctl_elem_list_get_id(list, i, id);
      const char *name = snd_ctl_elem_id_get_name(id);
      snd_ctl_elem_info_t *info;
      snd_ctl_elem_info_alloca(&info);
      snd_ctl_elem_info_set_id(info, id);
      snd_ctl_elem_info(ctl, info);
      snd_ctl_elem_value_t *value;
      snd_ctl_elem_value_alloca(&value);
      snd_ctl_elem_value_set_id(value, id);
      snd_ctl_elem_read(ctl, value);
      if (strstr(name, "Playback")) {
        snd_ctl_elem_type_t type = snd_ctl_elem_info_get_type(info);
        for (unsigned int j = 0; j < snd_ctl_elem_info_get_count(info); j++) {
          if (type == SND_CTL_ELEM_TYPE_BOOLEAN)
            snd_ctl_elem_value_set_boolean(value, j, 1);
          if (type == SND_CTL_ELEM_TYPE_INTEGER)
            snd_ctl_elem_value_set_integer(value, j,
                                           snd_ctl_elem_info_get_max(info));
        }
        snd_ctl_elem_write(ctl, value);
      }
      printf("CONTROL %s\n", name);
    }
    snd_ctl_elem_list_free_space(list);
    snd_ctl_close(ctl);
  }
  snd_pcm_t *pcm = NULL;
  int error = snd_pcm_open(&pcm, "test", SND_PCM_STREAM_PLAYBACK, 0);
  if (!error)
    error =
        snd_pcm_set_params(pcm, SND_PCM_FORMAT_S16_LE,
                           SND_PCM_ACCESS_RW_INTERLEAVED, 2, 48000, 1, 40000);
  if (error) {
    printf("ALSA ERROR %s\n", snd_strerror(error));
    fflush(stdout);
    for (;;)
      sleep(60);
  }
  int serial = open("/dev/ttyS0", O_RDWR | O_NOCTTY | O_NONBLOCK);
  struct termios term;
  if (serial < 0 || tcgetattr(serial, &term)) {
    printf("GLISS_AUDIO_MARKER_ERROR serial open\n");fflush(stdout);return 1;
  }
  cfmakeraw(&term);
  term.c_cflag |= CREAD | CLOCAL;
  if (tcsetattr(serial, TCSANOW, &term)) {
    printf("GLISS_AUDIO_MARKER_ERROR serial setup\n");fflush(stdout);return 1;
  }
  printf("GLISS_AUDIO_MARKER_READY 48000Hz stereo S16LE codes1-48 duration160ms\n");
  fflush(stdout);
  unsigned long sample = 0;
  short data[1920];
  for (;;) {
    commands(serial, sample);
    for (int n = 0; n < 960; n++, sample++) {
      double left = remaining ? 1000 + (current_code - 1) * 100 : 440;
      double right = remaining ? 11000 - (current_code - 1) * 100 : 880;
      data[n * 2] = 12000 * sin(2 * M_PI * left * sample / 48000.);
      data[n * 2 + 1] = 8000 * sin(2 * M_PI * right * sample / 48000.);
      if (remaining) remaining--;
    }
    int offset = 0;
    while (offset < 960) {
      snd_pcm_sframes_t n =
          snd_pcm_writei(pcm, data + offset * 2, 960 - offset);
      if (n < 0) {
        snd_pcm_recover(pcm, n, 1);
        continue;
      }
      offset += n;
    }
  }
}
