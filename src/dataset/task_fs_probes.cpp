#include "task_fs_internal.hpp"

extern "C" int capturefab_task_fs_probes(void *handle, char **error) noexcept {
    return capturefab::Boundary(error, [&] {
#ifndef _WIN32
        using namespace capturefab;
        auto space = *static_cast<NamespaceRef *>(handle);
        BoundedLocalFileSystem fs(space);
        auto &budget = *space->budget;
        auto require = [](bool condition, const char *message) { if (!condition) throw std::runtime_error(message); };
        auto open = [&](const char *name, bool append = false) {
            auto flags = FileFlags::FILE_FLAGS_READ | FileFlags::FILE_FLAGS_WRITE | FileFlags::FILE_FLAGS_FILE_CREATE;
            if (append) flags |= FileFlags::FILE_FLAGS_APPEND;
            return fs.OpenFile(space->path + "/" + name, flags);
        };
        auto remove = [&](const char *name) { fs.RemoveFile(space->path + "/" + name); };
        auto refused = [&](auto operation) { try { operation(); } catch (const duckdb::IOException &) { return true; } return false; };
        require(budget.limit == 128 && budget.used == 0, "probe requires a fresh 128-byte budget");
        char bytes[128] = {};
        auto database = open("task.duckdb");
        auto wal = open("task.duckdb.wal", true);
        fs.Write(*database, bytes, 60, 0);
        require(fs.Write(*wal, bytes, 60) == 60, "WAL write failed");
        require(refused([&] { fs.Write(*wal, bytes, 9); }), "combined DB/WAL ceiling was not enforced before append");
        require(fs.GetFileSize(*wal) == 60 && budget.used == 120, "refused append changed file or accounting");
        remove("task.duckdb");
        require(budget.used == 120, "unlink prematurely credited an open file");
        database->Close();
        require(budget.used == 60, "last close failed to credit an unlinked file");
        auto spill = open("spill.tmp");
        require(refused([&] { fs.Truncate(*spill, 69); }), "extending truncate bypassed the shared ceiling");
        require(fs.GetFileSize(*spill) == 0, "refused truncate extended the file");
        fs.Truncate(*spill, 68);
        require(budget.used == 128, "extending truncate was not charged");
        fs.Truncate(*spill, 2);
        require(budget.used == 62, "successful truncate did not reconcile EOF");
        fs.MoveFile(space->path + "/task.duckdb.wal", space->path + "/task.duckdb.wal.checkpoint");
        fs.MoveFile(space->path + "/task.duckdb.wal.checkpoint", space->path + "/task.duckdb.wal.recovery");
        auto recovery = open("task.duckdb.wal.recovery", true);
        require(fs.Write(*recovery, bytes, 2) == 2 && budget.used == 64, "recovery rename lost file identity or append charge");
        remove("task.duckdb.wal.recovery");
        wal->Close();
        require(budget.used == 64, "first close credited another open handle");
        recovery->Close();
        require(budget.used == 2, "final recovery close retained a proven deleted file");
        spill->Close(); remove("spill.tmp");
        require(budget.used == 0, "basic cleanup leaked charges");

        auto left = open("left"); auto right = open("right");
        std::atomic<int> successes {0};
        auto writer = [&](FileHandle &file) { try { fs.Write(file, bytes, 80, 0); ++successes; } catch (const IOException &) {} };
        std::thread first([&] { writer(*left); }); std::thread second([&] { writer(*right); });
        first.join(); second.join();
        require(successes == 1 && fs.GetFileSize(*left) + fs.GetFileSize(*right) == 80 && budget.used == 80, "concurrent file extensions overspent the combined budget");
        left->Close(); right->Close(); remove("left"); remove("right");

        auto partial = open("partial");
        fs.fail_write_probe = 2;
        require(fs.Write(*partial, bytes, 100) == 50, "partial sequential write probe failed");
        require(budget.used == 100 && fs.GetFileSize(*partial) == 50, "partial write released an uncertain reservation");
        auto other = open("other");
        require(refused([&] { fs.Truncate(*other, 29); }), "another file spent an uncertain partial-write reservation");
        other->Close(); remove("other"); partial->Close(); remove("partial");
        require(budget.used == 0, "proven partial-file removal retained charges");

        partial = open("partial-position");
        fs.fail_write_probe = 1;
        require(refused([&] { fs.Write(*partial, bytes, 100, 0); }), "partial positional failure was not injected");
        require(budget.used == 100 && fs.GetFileSize(*partial) == 3, "failed positional write discarded its reservation");
        partial->Close(); remove("partial-position");
        auto truncated = open("truncate-failure"); fs.Truncate(*truncated, 20);
        fs.fail_truncate_probe = true;
        require(refused([&] { fs.Truncate(*truncated, 100); }), "truncate failure was not injected");
        require(budget.used == 100 && fs.GetFileSize(*truncated) == 20, "uncertain truncate discarded its reservation");
        truncated->Close(); remove("truncate-failure");

        require(::symlinkat("/etc/hosts", space->root.value, "symlink") == 0, "symlink probe setup failed");
        require(refused([&] { open("symlink"); }), "file open followed a symlink");
        require(::unlinkat(space->root.value, "symlink", 0) == 0, "symlink cleanup failed");
        require(::symlinkat("/etc", space->root.value, "directory-symlink") == 0, "directory symlink setup failed");
        require(refused([&] { open("directory-symlink/hosts"); }), "file open followed a parent directory symlink");
        require(::unlinkat(space->root.value, "directory-symlink", 0) == 0, "directory symlink cleanup failed");
        auto original = open("hardlink-original"); fs.Write(*original, bytes, 4, 0);
        require(::linkat(space->root.value, "hardlink-original", space->root.value, "hardlink-alias", 0) == 0, "hardlink setup failed");
        require(refused([&] { fs.OpenFile(space->path + "/hardlink-alias", FileFlags::FILE_FLAGS_WRITE | FileFlags::FILE_FLAGS_FILE_CREATE_NEW); }), "destructive open accepted a multiply linked file");
        require(fs.GetFileSize(*original) == 4, "hardlink refusal happened after destructive truncate");
        require(::unlinkat(space->root.value, "hardlink-alias", 0) == 0, "hardlink alias cleanup failed");
        original->Close(); remove("hardlink-original");
        auto moved = space->name + "-moved";
        auto outside = space->name + "-outside";
        require(::renameat(space->parent.value, space->name.c_str(), space->parent.value, moved.c_str()) == 0, "namespace move failed");
        require(::mkdirat(space->parent.value, outside.c_str(), 0700) == 0, "outside probe directory failed");
        require(::symlinkat(outside.c_str(), space->parent.value, space->name.c_str()) == 0, "namespace alias probe failed");
        auto anchored = open("anchored"); fs.Write(*anchored, bytes, 4, 0);
        Fd outside_fd(::openat(space->parent.value, outside.c_str(), O_RDONLY | O_DIRECTORY | O_NOFOLLOW));
        struct stat absent;
        require(::fstatat(outside_fd.value, "anchored", &absent, AT_SYMLINK_NOFOLLOW) < 0 && errno == ENOENT, "namespace replacement redirected a write");
        anchored->Close(); remove("anchored");
        require(::unlinkat(space->parent.value, space->name.c_str(), 0) == 0, "namespace alias cleanup failed");
        require(::renameat(space->parent.value, moved.c_str(), space->parent.value, space->name.c_str()) == 0, "namespace restore failed");
        require(::unlinkat(space->parent.value, outside.c_str(), AT_REMOVEDIR) == 0, "outside probe cleanup failed");

        auto uncertain = open("uncertain-close"); fs.Truncate(*uncertain, 20);
        BoundedLocalFileSystem::Get(*uncertain).fail_close_probe = true;
        require(refused([&] { uncertain->Close(); }), "uncertain close was not injected");
        remove("uncertain-close");
        require(budget.used == 20, "uncertain final close released its charge");
        auto blocked = open("blocked"); budget.poisoned = true;
        require(refused([&] { fs.Write(*blocked, bytes, 1, 0); }), "poisoned accounting allowed a write");
        require(fs.GetFileSize(*blocked) == 0 && budget.peak <= budget.limit, "failed-closed probe changed the file or exceeded the cap");
        blocked->Close();
#else
        throw std::runtime_error("isolated task filesystem probes unsupported on this platform");
#endif
    });
}
