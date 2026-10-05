/* fts.h — fts.dll C ABI(api-spec 第 5 节子集,P3 手写版;结构变化时看 fts_abi_version)
 *
 * 内存规则:
 *   - fts_version() 返回静态字符串,勿释放;
 *   - fts_last_error() 返回值在下一次任何 fts_* 调用前有效,需保留必须拷贝;
 *   - 所有传入字符串为 UTF-8,NUL 结尾。
 */
#ifndef FTS_H
#define FTS_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

FTS_API const char* fts_version(void);
FTS_API uint32_t    fts_abi_version(void);
FTS_API const char* fts_last_error(void);

/* 阻塞式发送。
 * 返回 0 = 成功且校验通过;否则为契约退出码:
 *   1 通用 / 2 参数 / 3 连接 / 4 校验失败(数据不可信) / 5 中断 / 6 取消 / 7 认证
 */
FTS_API int32_t fts_send_file(const char* src_path,
                              const char* dst_addr,
                              const char* fingerprint_hex, /* 可为 NULL */
                              int32_t     insecure,        /* 非 0 跳过校验(仅测试) */
                              uint32_t    streams,         /* 0 = 4 */
                              uint32_t    chunk_size,      /* 0 = 8 MiB */
                              const char* transport);      /* NULL/"auto"/"quic"/"tcp" */

#ifdef __cplusplus
}
#endif
#endif /* FTS_H */
