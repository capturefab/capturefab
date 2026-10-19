#pragma once
#include "duckdb.h"
#include <cstdint>
extern "C" {
struct capturefab_task_usage {
    uint64_t limit, used, peak, denied, uncertain_files, poisoned;
    uint64_t spill_opens, wal_opens, recovery_opens;
};
int capturefab_task_budget_new(uint64_t limit, void **out, char **error) noexcept;
void capturefab_task_budget_free(void *budget) noexcept;
int capturefab_task_budget_usage(void *budget, capturefab_task_usage *out, char **error) noexcept;
int capturefab_task_namespace_new(const char *path, uint64_t expected_device, uint64_t expected_inode,
                                 void *budget, void **out, char **error) noexcept;
void capturefab_task_namespace_free(void *space) noexcept;
int capturefab_task_namespace_remove(void *space, char **error) noexcept;
int capturefab_task_database_open(void *space, duckdb_database *out, char **error) noexcept;
int capturefab_task_database_close(duckdb_database *database, char **error) noexcept;
int capturefab_task_fs_probes(void *space, char **error) noexcept;
}
