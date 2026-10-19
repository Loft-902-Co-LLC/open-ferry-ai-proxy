/* Not upstream's: install a raw generic-write ACE using the macOS SDK layout. */
#include <sys/types.h>
#include <sys/acl.h>
#include <sys/kauth.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <membership.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 1;
    int fd = open(argv[1], O_RDONLY | O_DIRECTORY);
    size_t size = KAUTH_FILESEC_SIZE(1);
    struct kauth_filesec *raw = calloc(1, size);
    filesec_t security = filesec_init();
    int result = 1;
    if (fd < 0 || raw == NULL || security == NULL) goto done;
    raw->fsec_magic = KAUTH_FILESEC_MAGIC;
    raw->fsec_entrycount = 1;
    raw->fsec_ace[0].ace_flags = KAUTH_ACE_PERMIT;
    raw->fsec_ace[0].ace_rights = KAUTH_ACE_GENERIC_WRITE;
    uid_t other = geteuid() == 65534 ? 65533 : 65534;
    if (mbr_uid_to_uuid(other, (unsigned char *)&raw->fsec_ace[0].ace_applicable) != 0) goto done;
    if (filesec_set_property(security, FILESEC_ACL_RAW, &raw) != 0) goto done;
    raw = NULL; /* filesec_free now owns the allocation. */
    mode_t mode = 0700;
    if (filesec_set_property(security, FILESEC_ACL_ALLOCSIZE, &size) != 0 ||
        filesec_set_property(security, FILESEC_MODE, &mode) != 0 ||
        fchmodx_np(fd, security) != 0) goto done;
    result = 0;
done:
    if (result != 0) perror("generic ACL fixture");
    free(raw);
    if (security != NULL) filesec_free(security);
    if (fd >= 0) close(fd);
    return result;
}
