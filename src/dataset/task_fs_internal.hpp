// Private implementation for the version-pinned task filesystem bridge.
#pragma once
#include "task_fs.h"
#include "duckdb/main/config.hpp"
#include "duckdb/common/virtual_file_system.hpp"
#include "duckdb/common/types/timestamp.hpp"
#include <algorithm>
#include <atomic>
#include <cerrno>
#include <cstring>
#include <exception>
#include <limits>
#include <map>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>
#ifndef _WIN32
#include <dirent.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>
#endif

namespace capturefab {
using namespace duckdb;
inline void Error(char **error, const char *message) noexcept {
    if (!error) return;
    *error = static_cast<char *>(duckdb_malloc(std::strlen(message) + 1));
    if (*error) std::strcpy(*error, message);
}
template <class F> int Boundary(char **error, F operation) noexcept {
    try { operation(); return 0; }
    catch (const std::exception &exception) { Error(error, exception.what()); }
    catch (...) { Error(error, "unknown isolated task filesystem failure"); }
    return 1;
}
struct Record {
    uint64_t extent = 0, handles = 0;
    bool linked = true, uncertain = false;
};
using Identity = std::pair<uint64_t, uint64_t>;
struct Budget {
    explicit Budget(uint64_t limit) : limit(limit) {}
    std::mutex mutex;
    std::atomic<bool> poisoned {false};
    const uint64_t limit;
    uint64_t used = 0, peak = 0, denied = 0;
    uint64_t spill_opens = 0, wal_opens = 0, recovery_opens = 0;
    std::map<Identity, std::shared_ptr<Record>> files;
    void Check() {
        if (poisoned) throw IOException("isolated task extent accounting is unavailable; storage is closed");
    }
    void Reserve(Record &record, uint64_t extent) {
        Check();
        if (extent <= record.extent) return;
        auto delta = extent - record.extent;
        if (delta > limit - used) { ++denied; throw IOException("isolated task logical file extent limit exceeded"); }
        used += delta;
        peak = std::max(peak, used);
        record.extent = extent;
    }
    void Shrink(Record &record, uint64_t extent) {
        if (extent > record.extent) { poisoned = true; throw IOException("task file grew outside its extent reservation"); }
        used -= record.extent - extent;
        record.extent = extent;
    }
    void Collect(const Identity &id, const std::shared_ptr<Record> &record) {
        if (!record->linked && !record->handles && !record->uncertain) {
            Shrink(*record, 0);
            files.erase(id);
        }
    }
};
using BudgetRef = std::shared_ptr<Budget>;

#ifndef _WIN32
inline void IoError(const char *operation) {
    throw IOException("isolated task %s failed: %s", operation, std::strerror(errno));
}
struct Fd {
    int value;
    explicit Fd(int fd = -1) : value(fd) {}
    Fd(const Fd &) = delete;
    Fd &operator=(const Fd &) = delete;
    Fd(Fd &&other) noexcept : value(other.Release()) {}
    Fd &operator=(Fd &&other) noexcept {
        if (value >= 0) ::close(value);
        value = other.Release();
        return *this;
    }
    ~Fd() { if (value >= 0) ::close(value); }
    int Release() { int result = value; value = -1; return result; }
};
inline struct stat Stat(int fd) {
    struct stat result;
    if (::fstat(fd, &result)) IoError("file stat");
    return result;
}
inline Identity Id(const struct stat &stat) { return {uint64_t(stat.st_dev), uint64_t(stat.st_ino)}; }
inline std::vector<std::string> Components(const std::string &path) {
    std::vector<std::string> result;
    size_t start = 0;
    while (start < path.size()) {
        auto end = path.find('/', start);
        if (end == std::string::npos) end = path.size();
        auto component = path.substr(start, end - start);
        if (!component.empty()) {
            if (component == "." || component == ".." || component.find(':') != std::string::npos ||
                std::any_of(component.begin(), component.end(), [](unsigned char c) { return c < 32; })) {
                throw IOException("isolated task path requires ordinary components");
            }
            result.push_back(component);
        }
        start = end + 1;
    }
    return result;
}
struct Namespace {
    std::string path, name;
    Fd root, parent;
    BudgetRef budget;
    Namespace(const std::string &path, Identity expected, BudgetRef budget) : path(path), budget(std::move(budget)) {
        if (path.empty() || path[0] != '/') throw IOException("task namespace must be absolute");
        auto parts = Components(path);
        if (parts.empty()) throw IOException("task namespace cannot be the filesystem root");
        Fd current(::open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
        if (current.value < 0) IoError("root open");
        for (size_t i = 0; i < parts.size(); ++i) {
            Fd child(::openat(current.value, parts[i].c_str(), O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC));
            if (child.value < 0) IoError("namespace ancestry open");
            if (i + 1 == parts.size()) { parent = std::move(current); root = std::move(child); name = parts[i]; }
            else current = std::move(child);
        }
        const auto stat = Stat(root.value);
        if (Id(stat) != expected) throw IOException("task namespace identity changed before native admission");
        if (stat.st_uid != ::geteuid() || (stat.st_mode & 0077)) throw IOException("task namespace must be private to its owner");
    }
    std::vector<std::string> Relative(const std::string &candidate) {
        if (candidate == path) return {};
        if (candidate.size() <= path.size() || candidate.compare(0, path.size(), path) || candidate[path.size()] != '/') {
            throw IOException("file is outside the isolated task namespace");
        }
        return Components(candidate.substr(path.size() + 1));
    }
    struct Parent { Fd fd; std::string name; };
    Parent Resolve(const std::string &candidate) {
        auto parts = Relative(candidate);
        if (parts.empty()) throw IOException("operation requires a file inside the task namespace");
        Fd current(::dup(root.value));
        if (current.value < 0) IoError("namespace descriptor duplicate");
        for (size_t i = 0; i + 1 < parts.size(); ++i) {
            Fd child(::openat(current.value, parts[i].c_str(), O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC));
            if (child.value < 0) IoError("directory open");
            current = std::move(child);
        }
        return {std::move(current), parts.back()};
    }
    Fd Directory(const std::string &candidate) {
        if (Relative(candidate).empty()) { Fd fd(::dup(root.value)); if (fd.value < 0) IoError("directory duplicate"); return fd; }
        auto parent = Resolve(candidate);
        Fd fd(::openat(parent.fd.value, parent.name.c_str(), O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC));
        if (fd.value < 0) IoError("directory open");
        return fd;
    }
    std::shared_ptr<Record> Register(const struct stat &stat) {
        if (!S_ISREG(stat.st_mode) || stat.st_nlink > 1 || stat.st_size < 0) throw IOException("task storage requires ordinary files with one link");
        const auto id = Id(stat);
        auto found = budget->files.find(id);
        auto record = found == budget->files.end() ? std::make_shared<Record>() : found->second;
        budget->Reserve(*record, uint64_t(stat.st_size));
        budget->files[id] = record;
        return record;
    }
};
using NamespaceRef = std::shared_ptr<Namespace>;
struct Handle final : FileHandle {
    NamespaceRef space;
    Fd fd;
    Identity identity;
    std::shared_ptr<Record> record;
    bool fail_close_probe = false;
    Handle(FileSystem &fs, const std::string &path, FileOpenFlags flags, NamespaceRef space, Fd fd,
           Identity identity, std::shared_ptr<Record> record)
        : FileHandle(fs, path, flags), space(std::move(space)), fd(std::move(fd)), identity(identity), record(std::move(record)) {}
    ~Handle() override { try { Close(); } catch (...) { space->budget->poisoned = true; } }
    void Close() override {
        if (fd.value < 0) return;
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        int closing = fd.Release();
        if (::close(closing) || fail_close_probe) { record->uncertain = true; throw IOException("task close result is uncertain"); }
        --record->handles;
        if (!record->linked && !record->handles) record->uncertain = false;
        budget.Collect(identity, record);
    }
};

// Installed as VirtualFileSystem's default local adapter. Every native handle
// stays wrapped; IsLocalFileSystem prevents DuckDB's uncapped local fallback.
class BoundedLocalFileSystem final : public FileSystem {
public:
    explicit BoundedLocalFileSystem(NamespaceRef space) : space(std::move(space)) {}
    NamespaceRef space;
    // Only the native regression probe sets these; no SQL/production API can.
    int fail_write_probe = 0;
    bool fail_truncate_probe = false;
    bool IsLocalFileSystem() const override { return true; }
    std::string GetName() const override { return "CapturefabTaskLocalFileSystem"; }
    bool CanSeek() override { return true; }
    bool OnDiskFile(FileHandle &) override { return true; }
    bool IsPipe(const string &path, optional_ptr<FileOpener> = nullptr) override { space->Relative(path); return false; }
    string CanonicalizePath(const string &path, optional_ptr<FileOpener> = nullptr) override { space->Relative(path); return path; }
    unique_ptr<FileHandle> OpenFile(const string &path, FileOpenFlags flags, optional_ptr<FileOpener> = nullptr) override {
        auto parent = space->Resolve(path);
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        budget.Check();
        if (flags.DirectIO()) throw IOException("direct I/O is unsupported in isolated task storage");
        int mode = flags.OpenForWriting() ? (flags.OpenForReading() ? O_RDWR : O_WRONLY) : O_RDONLY;
        mode |= O_NOFOLLOW | O_CLOEXEC;
        if (flags.CreateFileIfNotExists() || flags.OverwriteExistingFile() || flags.ExclusiveCreate()) mode |= O_CREAT;
        if (flags.ExclusiveCreate() || flags.ReturnNullIfExists()) mode |= O_EXCL | O_CREAT;
        if (flags.OpenForAppending()) mode |= O_APPEND;
        Fd fd(::openat(parent.fd.value, parent.name.c_str(), mode, 0600));
        if (fd.value < 0) {
            if ((errno == ENOENT && flags.ReturnNullIfNotExists()) || (errno == EEXIST && flags.ReturnNullIfExists())) return nullptr;
            IoError("file open");
        }
        auto stat = Stat(fd.value);
        auto record = space->Register(stat);
        if (path.find(space->path + "/spill/") == 0) ++budget.spill_opens;
        if (path.size() >= 4 && path.compare(path.size() - 4, 4, ".wal") == 0) ++budget.wal_opens;
        if (path.find(".wal.checkpoint") != string::npos || path.find(".wal.recovery") != string::npos) ++budget.recovery_opens;
        if (flags.OverwriteExistingFile()) {
            // Validate identity/link count before destructive truncation.
            if (::ftruncate(fd.value, 0)) { record->uncertain = true; IoError("overwrite truncate"); }
            budget.Shrink(*record, 0);
            record->uncertain = false;
        }
        if (flags.Lock() != FileLockType::NO_LOCK) {
            struct flock lock = {};
            lock.l_type = flags.Lock() == FileLockType::READ_LOCK ? F_RDLCK : F_WRLCK;
            lock.l_whence = SEEK_SET;
            if (::fcntl(fd.value, F_SETLK, &lock)) IoError("file lock");
        }
        ++record->handles;
        return make_uniq<Handle>(*this, path, flags, space, std::move(fd), Id(stat), record);
    }
    static Handle &Get(FileHandle &handle) { return static_cast<Handle &>(handle); }
    int64_t Read(FileHandle &base, void *buffer, int64_t bytes) override {
        std::lock_guard<std::mutex> guard(space->budget->mutex);
        if (bytes < 0) throw IOException("negative task read size");
        auto result = ::read(Get(base).fd.value, buffer, size_t(bytes));
        if (result < 0) IoError("read");
        return result;
    }
    void Read(FileHandle &base, void *buffer, int64_t bytes, idx_t location) override {
        if (bytes < 0 || location > uint64_t(std::numeric_limits<int64_t>::max()) - uint64_t(bytes)) throw IOException("task read size overflow");
        int64_t done = 0;
        while (done < bytes) {
            auto result = ::pread(Get(base).fd.value, static_cast<char *>(buffer) + done, size_t(bytes - done), off_t(location + done));
            if (result < 0 && errno == EINTR) continue;
            if (result <= 0) { if (!result) throw IOException("unexpected end of task file"); IoError("positional read"); }
            done += result;
        }
    }
    uint64_t End(Handle &handle, int64_t bytes, uint64_t location) {
        if (bytes < 0 || location > uint64_t(std::numeric_limits<int64_t>::max()) - uint64_t(bytes)) throw IOException("task write size overflow");
        if (handle.flags.OpenForAppending()) location = std::max(location, uint64_t(Stat(handle.fd.value).st_size));
        if (location > uint64_t(std::numeric_limits<int64_t>::max()) - uint64_t(bytes)) throw IOException("task append size overflow");
        return location + uint64_t(bytes);
    }
    void Write(FileHandle &base, void *buffer, int64_t bytes, idx_t location) override {
        auto &handle = Get(base);
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        budget.Reserve(*handle.record, End(handle, bytes, location));
        try {
            if (fail_write_probe == 1) {
                fail_write_probe = 0;
                if (::pwrite(handle.fd.value, buffer, size_t(std::min<int64_t>(bytes, 3)), off_t(location)) < 0) IoError("partial write probe");
                throw IOException("injected failure after a partial positional write");
            }
            int64_t done = 0;
            while (done < bytes) {
                auto result = ::pwrite(handle.fd.value, static_cast<char *>(buffer) + done, size_t(bytes - done), off_t(location + done));
                if (result < 0 && errno == EINTR) continue;
                if (result <= 0) { if (!result) throw IOException("task positional write made no progress"); IoError("positional write"); }
                done += result;
            }
        } catch (...) { handle.record->uncertain = true; throw; }
    }
    int64_t Write(FileHandle &base, void *buffer, int64_t bytes) override {
        auto &handle = Get(base);
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        auto position = ::lseek(handle.fd.value, 0, SEEK_CUR);
        if (position < 0) IoError("write position");
        budget.Reserve(*handle.record, End(handle, bytes, uint64_t(position)));
        auto written_bytes = fail_write_probe == 2 ? bytes / 2 : bytes;
        fail_write_probe = 0;
        auto result = ::write(handle.fd.value, buffer, size_t(written_bytes));
        if (result < 0) { handle.record->uncertain = true; IoError("write"); }
        if (result < bytes) handle.record->uncertain = true;
        return result;
    }
    void Truncate(FileHandle &base, int64_t size) override {
        if (size < 0) throw IOException("negative task truncate size");
        auto &handle = Get(base);
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        budget.Reserve(*handle.record, uint64_t(size));
        if (fail_truncate_probe) { fail_truncate_probe = false; handle.record->uncertain = true; throw IOException("injected uncertain truncate failure"); }
        if (::ftruncate(handle.fd.value, off_t(size))) { handle.record->uncertain = true; IoError("truncate"); }
        auto stat = Stat(handle.fd.value);
        if (stat.st_size != size) { handle.record->uncertain = true; throw IOException("task truncate result is uncertain"); }
        budget.Shrink(*handle.record, uint64_t(size));
        handle.record->uncertain = false;
    }
    int64_t GetFileSize(FileHandle &handle) override { return Stat(Get(handle).fd.value).st_size; }
    FileType GetFileType(FileHandle &) override { return FileType::FILE_TYPE_REGULAR; }
    timestamp_t GetLastModifiedTime(FileHandle &handle) override { return Timestamp::FromEpochSeconds(Stat(Get(handle).fd.value).st_mtime); }
    string GetVersionTag(FileHandle &handle) override {
        auto stat = Stat(Get(handle).fd.value);
        return std::to_string(stat.st_dev) + ":" + std::to_string(stat.st_ino) + ":" + std::to_string(stat.st_size) + ":" + std::to_string(stat.st_mtime);
    }
    void Seek(FileHandle &handle, idx_t location) override {
        std::lock_guard<std::mutex> guard(space->budget->mutex);
        if (location > uint64_t(std::numeric_limits<int64_t>::max()) || ::lseek(Get(handle).fd.value, off_t(location), SEEK_SET) < 0) IoError("seek");
    }
    idx_t SeekPosition(FileHandle &handle) override {
        std::lock_guard<std::mutex> guard(space->budget->mutex);
        auto result = ::lseek(Get(handle).fd.value, 0, SEEK_CUR);
        if (result < 0) IoError("seek position");
        return idx_t(result);
    }
    void FileSync(FileHandle &handle) override { if (::fsync(Get(handle).fd.value)) IoError("sync"); }
    bool Trim(FileHandle &, idx_t, idx_t) override { return false; }
    bool DirectoryExists(const string &path, optional_ptr<FileOpener> = nullptr) override {
        if (space->Relative(path).empty()) return true;
        auto parent = space->Resolve(path);
        struct stat stat;
        if (::fstatat(parent.fd.value, parent.name.c_str(), &stat, AT_SYMLINK_NOFOLLOW)) { if (errno == ENOENT) return false; IoError("directory stat"); }
        if (S_ISLNK(stat.st_mode)) throw IOException("task directory cannot be a symlink");
        return S_ISDIR(stat.st_mode);
    }
    void CreateDirectory(const string &path, optional_ptr<FileOpener> = nullptr) override {
        auto parent = space->Resolve(path);
        if (::mkdirat(parent.fd.value, parent.name.c_str(), 0700)) IoError("directory create");
    }
    bool FileExists(const string &path, optional_ptr<FileOpener> = nullptr) override {
        auto parent = space->Resolve(path);
        struct stat stat;
        if (::fstatat(parent.fd.value, parent.name.c_str(), &stat, AT_SYMLINK_NOFOLLOW)) { if (errno == ENOENT) return false; IoError("file stat"); }
        if (S_ISLNK(stat.st_mode)) throw IOException("task file cannot be a symlink");
        return S_ISREG(stat.st_mode);
    }
    bool ListFiles(const string &path, const std::function<void(const string &, bool)> &callback, FileOpener * = nullptr) override {
        auto fd = space->Directory(path);
        auto directory = ::fdopendir(fd.Release());
        if (!directory) IoError("directory listing open");
        std::vector<std::pair<std::string, bool>> entries;
        try {
            errno = 0;
            while (auto entry = ::readdir(directory)) {
                const std::string name(entry->d_name);
                if (name == "." || name == "..") continue;
                struct stat stat;
                if (::fstatat(::dirfd(directory), name.c_str(), &stat, AT_SYMLINK_NOFOLLOW)) IoError("listing stat");
                if (S_ISLNK(stat.st_mode)) throw IOException("task namespace contains a symlink");
                entries.emplace_back(name, S_ISDIR(stat.st_mode));
                errno = 0;
            }
            if (errno) IoError("directory listing");
        } catch (...) { ::closedir(directory); throw; }
        if (::closedir(directory)) IoError("directory listing close");
        for (auto &entry : entries) callback(entry.first, entry.second);
        return true;
    }
    bool TryRemoveFile(const string &path, optional_ptr<FileOpener> = nullptr) override {
        auto parent = space->Resolve(path);
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        struct stat stat;
        if (::fstatat(parent.fd.value, parent.name.c_str(), &stat, AT_SYMLINK_NOFOLLOW)) { if (errno == ENOENT) return false; IoError("remove stat"); }
        auto record = space->Register(stat);
        if (::unlinkat(parent.fd.value, parent.name.c_str(), 0)) { record->uncertain = true; IoError("unlink"); }
        record->linked = false;
        if (!record->handles) record->uncertain = false;
        budget.Collect(Id(stat), record);
        return true;
    }
    void RemoveFile(const string &path, optional_ptr<FileOpener> opener = nullptr) override {
        if (!TryRemoveFile(path, opener)) throw IOException("task file to remove is missing");
    }
    void RemoveDirectory(const string &path, optional_ptr<FileOpener> = nullptr) override {
        ListFiles(path, [&](const string &name, bool directory) {
            auto child = path + "/" + name;
            if (directory) RemoveDirectory(child); else RemoveFile(child);
        });
        auto parent = space->Resolve(path);
        if (::unlinkat(parent.fd.value, parent.name.c_str(), AT_REMOVEDIR)) IoError("directory remove");
    }
    void MoveFile(const string &source, const string &target, optional_ptr<FileOpener> = nullptr) override {
        auto from = space->Resolve(source);
        auto to = space->Resolve(target);
        auto &budget = *space->budget;
        std::lock_guard<std::mutex> guard(budget.mutex);
        struct stat source_stat, target_stat;
        if (::fstatat(from.fd.value, from.name.c_str(), &source_stat, AT_SYMLINK_NOFOLLOW)) IoError("rename source stat");
        space->Register(source_stat);
        std::shared_ptr<Record> replaced;
        if (!::fstatat(to.fd.value, to.name.c_str(), &target_stat, AT_SYMLINK_NOFOLLOW)) replaced = space->Register(target_stat);
        else if (errno != ENOENT) IoError("rename target stat");
        if (::renameat(from.fd.value, from.name.c_str(), to.fd.value, to.name.c_str())) IoError("rename");
        if (replaced && Id(source_stat) != Id(target_stat)) {
            replaced->linked = false;
            if (!replaced->handles) replaced->uncertain = false;
            budget.Collect(Id(target_stat), replaced);
        }
    }
    vector<OpenFileInfo> Glob(const string &path, FileOpener * = nullptr) override {
        if (FileSystem::HasGlob(path)) throw IOException("task storage does not allow filesystem glob patterns");
        vector<OpenFileInfo> result;
        if (FileExists(path)) result.emplace_back(path);
        return result;
    }
};
#else
struct Namespace {};
using NamespaceRef = std::shared_ptr<Namespace>;
#endif
} // namespace capturefab
