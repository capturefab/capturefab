#include "task_fs_internal.hpp"

extern "C" {
int capturefab_task_budget_new(uint64_t limit, void **out, char **error) noexcept {
    return capturefab::Boundary(error, [&] {
        if (!limit) throw std::runtime_error("task extent limit must be positive");
        *out = new capturefab::BudgetRef(std::make_shared<capturefab::Budget>(limit));
    });
}
void capturefab_task_budget_free(void *budget) noexcept { delete static_cast<capturefab::BudgetRef *>(budget); }
int capturefab_task_budget_usage(void *handle, capturefab_task_usage *out, char **error) noexcept {
    return capturefab::Boundary(error, [&] {
        auto &budget = **static_cast<capturefab::BudgetRef *>(handle);
        std::lock_guard<std::mutex> guard(budget.mutex);
        *out = {budget.limit, budget.used, budget.peak, budget.denied, 0, uint64_t(budget.poisoned.load()), budget.spill_opens, budget.wal_opens, budget.recovery_opens};
        for (auto &file : budget.files) if (file.second->uncertain) ++out->uncertain_files;
    });
}
int capturefab_task_namespace_new(const char *path, uint64_t expected_device, uint64_t expected_inode,
                                 void *budget, void **out, char **error) noexcept {
    return capturefab::Boundary(error, [&] {
#ifndef _WIN32
        *out = new capturefab::NamespaceRef(std::make_shared<capturefab::Namespace>(
            path, capturefab::Identity{expected_device, expected_inode},
            *static_cast<capturefab::BudgetRef *>(budget)));
#else
        throw std::runtime_error("isolated task filesystem is unsupported on Windows; shared datasets remain available");
#endif
    });
}
void capturefab_task_namespace_free(void *space) noexcept { delete static_cast<capturefab::NamespaceRef *>(space); }
int capturefab_task_namespace_remove(void *handle, char **error) noexcept {
    return capturefab::Boundary(error, [&] {
#ifndef _WIN32
        auto space = *static_cast<capturefab::NamespaceRef *>(handle);
        capturefab::BoundedLocalFileSystem fs(space);
        fs.ListFiles(space->path, [&](const duckdb::string &name, bool directory) {
            auto path = space->path + "/" + name;
            if (directory) fs.RemoveDirectory(path); else fs.RemoveFile(path);
        });
        struct stat named;
        if (::fstatat(space->parent.value, space->name.c_str(), &named, AT_SYMLINK_NOFOLLOW) || capturefab::Id(named) != capturefab::Id(capturefab::Stat(space->root.value))) {
            throw std::runtime_error("task namespace was moved; anchored contents removed but directory retained");
        }
        if (::unlinkat(space->parent.value, space->name.c_str(), AT_REMOVEDIR)) capturefab::IoError("namespace remove");
#else
        throw std::runtime_error("isolated task filesystem is unsupported on this platform");
#endif
    });
}
int capturefab_task_database_open(void *handle, duckdb_database *out, char **error) noexcept {
    duckdb_config config = nullptr;
    auto result = capturefab::Boundary(error, [&] {
#ifndef _WIN32
        if (std::strcmp(duckdb_library_version(), "v1.5.6")) throw std::runtime_error("task filesystem requires bundled DuckDB v1.5.6");
        auto space = *static_cast<capturefab::NamespaceRef *>(handle);
        if (duckdb_create_config(&config) != DuckDBSuccess) throw std::runtime_error("cannot create task database configuration");
        for (auto setting : {std::make_pair("autoload_known_extensions", "false"), {"autoinstall_known_extensions", "false"}, {"threads", "2"}, {"memory_limit", "32MiB"}}) {
            if (duckdb_set_config(config, setting.first, setting.second) != DuckDBSuccess) throw std::runtime_error("cannot configure isolated task database");
        }
        auto &cfg = *reinterpret_cast<duckdb::DBConfig *>(config);
        cfg.file_system = duckdb::make_uniq<duckdb::VirtualFileSystem>(duckdb::make_uniq<capturefab::BoundedLocalFileSystem>(space));
        char *open_error = nullptr;
        if (duckdb_open_ext((space->path + "/task.duckdb").c_str(), out, config, &open_error) != DuckDBSuccess) {
            auto message = std::string(open_error ? open_error : "cannot open isolated task database");
            if (open_error) duckdb_free(open_error);
            throw std::runtime_error(message);
        }
#else
        throw std::runtime_error("isolated task filesystem is unsupported on this platform");
#endif
    });
    if (config) duckdb_destroy_config(&config);
    return result;
}
int capturefab_task_database_close(duckdb_database *database, char **error) noexcept {
    return capturefab::Boundary(error, [&] { duckdb_close(database); });
}
} // extern C
