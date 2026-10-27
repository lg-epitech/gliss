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
  printf("GLISS_AUDIO_READY 48000Hz stereo S16LE 440Hz left 880Hz right\n");
  fflush(stdout);
  unsigned long sample = 0;
  short data[1920];
  for (;;) {
    for (int n = 0; n < 960; n++, sample++) {
      data[n * 2] = 12000 * sin(2 * M_PI * 440 * sample / 48000.);
      data[n * 2 + 1] = 8000 * sin(2 * M_PI * 880 * sample / 48000.);
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
