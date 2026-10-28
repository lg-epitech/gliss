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
