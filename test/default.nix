{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  sqlitefs,
}:
let
  nixos-lib = import (pkgs.path + "/nixos/lib") { inherit (pkgs) lib; };
in
nixos-lib.runTest {
  hostPkgs = pkgs;
  name = "sqlitefs";

  nodes.machine =
    { pkgs, ... }:
    {
      environment.systemPackages = [
        # Run the VM test against a DEBUG build so debug_assert!s (ours and
        # fuser's) are actually exercised; the main build stays release.
        (sqlitefs.overrideAttrs { cargoBuildType = "debug"; })
        pkgs.fuse3
        pkgs.sqlite
      ];
      boot.kernelModules = [ "fuse" ];
      virtualisation.diskSize = 1024;
    };

  testScript = ''
    # ---- init ----
    machine.succeed("mkdir -p /tmp/repo /tmp/mnt")
    machine.succeed("sqlitefs init /tmp/repo/test.db")

    # ---- mount ----
    machine.succeed("sqlitefs mount /tmp/repo/test.db /tmp/mnt > /tmp/mount.log 2>&1 &")
    machine.wait_until_succeeds("test -d /tmp/mnt/.sqlitefs/refs/main")
    machine.succeed("cat /tmp/mount.log || true")

    # ---- basic file ops ----
    machine.succeed("echo 'hello sqlitefs' > /tmp/mnt/file1.txt")
    machine.succeed("cat /tmp/mount.log || true")
    machine.succeed("cat /tmp/mount.log | grep -i 'failed\|error' || true")
    machine.succeed("test -f /tmp/mnt/file1.txt")
    machine.succeed("grep 'hello sqlitefs' /tmp/mnt/file1.txt")

    # ---- mkdir ----
    machine.succeed("mkdir /tmp/mnt/subdir")
    machine.succeed("test -d /tmp/mnt/subdir")

    # ---- symlink ----
    machine.succeed("ln -s /some/target /tmp/mnt/mylink")
    machine.succeed("test -L /tmp/mnt/mylink")
    machine.succeed("readlink /tmp/mnt/mylink | grep '/some/target'")

    # ---- rename ----
    machine.succeed("mv /tmp/mnt/file1.txt /tmp/mnt/subdir/renamed.txt")
    machine.succeed("test -f /tmp/mnt/subdir/renamed.txt")
    machine.fail("test -f /tmp/mnt/file1.txt")

    # ---- manual snapshot (tag) ----
    machine.succeed("touch /tmp/mnt/.sqlitefs/tag/test-snap")
    machine.succeed("test -d /tmp/mnt/.sqlitefs/refs/test-snap")
    machine.succeed("test -f /tmp/mnt/.sqlitefs/refs/test-snap/subdir/renamed.txt")

    # ---- executable bit + exec of a real binary ----

    # Copy pkgs.hello's /bin/hello (a dynamically-linked ELF) onto the
    # filesystem. cp -p preserves the source's 0755 mode, so this also
    # verifies that mode bits are carried through the write path.
    machine.succeed("cp -p ${pkgs.hello}/bin/hello /tmp/mnt/hello")
    machine.succeed("test -x /tmp/mnt/hello")
    machine.succeed("/tmp/mnt/hello")
    machine.succeed("/tmp/mnt/hello | grep 'Hello, world!'")

    # Explicit chmod path: copy without -p (comes in as 0644), chmod +x,
    # then execute. Exercises setattr's mode handling, not just the
    # source-mode passthrough.
    machine.succeed("cp ${pkgs.hello}/bin/hello /tmp/mnt/hello2")
    machine.fail("test -x /tmp/mnt/hello2")
    machine.succeed("chmod +x /tmp/mnt/hello2")
    machine.succeed("test -x /tmp/mnt/hello2")
    machine.succeed("/tmp/mnt/hello2 | grep 'Hello, world!'")

    # The chmod must survive a commit + remount (mode is part of the tree).
    machine.succeed("touch /tmp/mnt/.sqlitefs/tag/exec-check")
    machine.succeed("test -x /tmp/mnt/.sqlitefs/refs/exec-check/hello2")

    # ---- hash access ----
    # Use the deterministic test-snap ref (proven to contain subdir/renamed.txt)
    # instead of "latest commit": multiple commits share the same timestamp
    # second, so ORDER BY timestamp is nondeterministic among them.
    hex = machine.succeed(
      "sqlite3 /tmp/repo/test.db \"SELECT hex(commit_hash) FROM refs WHERE name = 'test-snap'\""
    ).strip()
    out = machine.succeed(f"ls -laR /tmp/mnt/.sqlitefs/c/{hex}/");
    print(out)
    machine.succeed(f"test -d /tmp/mnt/.sqlitefs/c/{hex}")
    machine.succeed(f"test -f /tmp/mnt/.sqlitefs/c/{hex}/subdir/renamed.txt")

    # ---- unlink ----
    machine.succeed("rm /tmp/mnt/mylink")
    machine.fail("test -L /tmp/mnt/mylink")

    # ---- empty file persists (touch foo must survive a commit) ----
    machine.succeed("touch /tmp/mnt/empty.txt")
    machine.succeed("test -f /tmp/mnt/empty.txt")
    machine.succeed("test ! -s /tmp/mnt/empty.txt")
    machine.succeed("touch /tmp/mnt/.sqlitefs/tag/empty-check")
    machine.succeed("test -f /tmp/mnt/.sqlitefs/refs/empty-check/empty.txt")

    # ---- truncate ----
    machine.succeed("echo 'new content' > /tmp/mnt/subdir/renamed.txt")
    machine.succeed("grep 'new content' /tmp/mnt/subdir/renamed.txt")

    # ---- unmount ----
    machine.succeed("fusermount3 -u /tmp/mnt")
    machine.succeed("cat /tmp/mount.log || true")

    # ---- remount and verify persistence ----
    machine.succeed("sqlitefs mount /tmp/repo/test.db /tmp/mnt > /tmp/mount.log 2>&1 &")
    machine.wait_until_succeeds("test -d /tmp/mnt/.sqlitefs/refs/main")
    # empty file (touch foo) survived the commit + remount
    machine.succeed("test -f /tmp/mnt/empty.txt")
    machine.succeed("test ! -s /tmp/mnt/empty.txt")
    machine.succeed("test -d /tmp/mnt/subdir")
    machine.succeed("test -f /tmp/mnt/subdir/renamed.txt")
    machine.succeed("grep 'new content' /tmp/mnt/subdir/renamed.txt")

    # ---- snapshot still there ----
    machine.succeed("test -d /tmp/mnt/.sqlitefs/refs/test-snap")
    machine.succeed("test -f /tmp/mnt/.sqlitefs/refs/test-snap/subdir/renamed.txt")
    machine.succeed("grep 'hello' /tmp/mnt/.sqlitefs/refs/test-snap/subdir/renamed.txt")

    # ---- show-ref lists main + the tagged snapshots ----
    refs = machine.succeed("sqlitefs show-ref /tmp/repo/test.db")
    assert " main" in refs, f"show-ref missing main: {refs}"
    assert " test-snap" in refs, f"show-ref missing test-snap: {refs}"
    assert " empty-check" in refs, f"show-ref missing empty-check: {refs}"
    assert " exec-check" in refs, f"show-ref missing exec-check: {refs}"

    # ---- executable bit survives remount (mode is committed with the tree) ----
    machine.succeed("test -x /tmp/mnt/hello")
    machine.succeed("test -x /tmp/mnt/hello2")
    machine.succeed("/tmp/mnt/hello2 | grep 'Hello, world!'")

    # ---- fsck after all ops (repo must be healthy) ----
    machine.succeed("sqlitefs fsck /tmp/repo/test.db")

    # ---- clean up ----
    machine.succeed("fusermount3 -u /tmp/mnt")
  '';
}
