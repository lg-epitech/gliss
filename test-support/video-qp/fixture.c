#define _GNU_SOURCE
// Reuse the guest's exact procedural pixels without booting or opening devices.
#define main unused_guest_main
#include "../qemu-audio-motion/init.c"
#undef main

int main(int argc, char **argv) {
  if (argc != 2) return 64;
  int fd = open(argv[1], O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0600);
  if (fd < 0) return 65;
  unsigned char *frame = malloc(1280 * 800 * 4);
  if (!frame) { close(fd); return 66; }
  for (unsigned int index = 0; index < 12; index++) {
    for (unsigned int y = 0; y < 800; y++) for (unsigned int x = 0; x < 1280; x++) {
      uint32_t color = x < 1024 && y < 768 ? pixel(x, y, index * 3) : 0;
      size_t offset = ((size_t)y * 1280 + x) * 4;
      frame[offset] = color >> 16; frame[offset + 1] = color >> 8;
      frame[offset + 2] = color; frame[offset + 3] = 255;
    }
    size_t offset = 0;
    while (offset < 1280 * 800 * 4) {
      ssize_t wrote = write(fd, frame + offset, 1280 * 800 * 4 - offset);
      if (wrote < 0 && errno == EINTR) continue;
      if (wrote <= 0) { free(frame);close(fd);return 67; }
      offset += wrote;
    }
  }
  free(frame);
  if (fsync(fd) || close(fd)) return 68;
  return 0;
}
