#!/usr/bin/env python3
"""Deterministic synthetic capacity fixtures, NOT production retrieval samples.

Two useful memories remain constant as hard negatives increase. All text is
under the production per-candidate truncation budget. No model-specific wording.
"""
import argparse
import json
from pathlib import Path


def build_cases():
    cases = []
    # Non-monotonic order avoids conflating batch size with progressive warmup.
    for size in (96, 6, 256, 24):
        for domain in ("metrics", "rollback"):
            if domain == "metrics":
                user = "For Orion production metrics scraping, give the exact metrics port and HTTP path. This is not a health check or another environment."
                relevant = ["Orion production metrics are exposed on port 9407.",
                            "Orion production metrics use the HTTP path /internal/orion-metrics."]
                negatives = [
                    f"Orion {env} {kind} use port {8000 + i} and path /{env}/{kind.replace(' ', '-')} ."
                    for i, (env, kind) in enumerate(
                        (env, kind) for env in ("staging", "development", "preview", "integration", "sandbox", "training")
                        for kind in ("metrics", "health checks", "admin API", "debug API"))]
                negatives += [f"{service} production metrics use port {9100+i} and path /metrics/{service.lower()}."
                              for i, service in enumerate(("Vega", "Atlas", "Boreal", "Nova", "Quasar", "Lyra"))]
                negatives += ["Orion production health checks use port 8088 and path /healthz.",
                              "Orion production metrics dashboards use the blue theme.",
                              "Orion production metrics presentations are scheduled on Friday."]
                # Additional distinct plausible project records, not repeated filler.
                negatives += [f"Orion preview environment pr-{i} metrics use port {10000+i} and path /preview-{i}/metrics."
                              for i in range(256)]
                contract = 'Return {"port":integer|null,"path":string|null}. Unknown project-specific values must be null.'
                answer = {"port": 9407, "path": "/internal/orion-metrics"}
            else:
                user = "给出发货服务生产发布前回滚演练的准确命令和要求保存的报告路径；不是支付服务，也不是预览环境。"
                relevant = ["发货服务生产发布前回滚演练执行 ./drills/shipping-rollback.sh。",
                            "发货服务生产发布前回滚演练报告保存到 artifacts/shipping-rehearsal.json。"]
                negatives = [f"{service}服务{env}发布前回滚演练执行 ./drills/{i}-rollback.sh，报告保存到 reports/{i}.json。"
                             for i, (service, env) in enumerate(
                                 (service, env) for service in ("支付", "库存", "订单", "账号", "发票", "通知")
                                 for env in ("生产", "预览", "开发", "集成"))]
                negatives += ["发货服务生产发布完成后执行 ./checks/shipping-health.sh。",
                              "发货服务生产发布前的评审会议使用中文纪要。",
                              "发货服务的产品介绍使用 PDF。"]
                negatives += [f"发货服务预览环境 pr-{i} 回滚演练执行 ./preview/{i}/rollback.sh，报告保存到 preview/{i}.json。"
                              for i in range(256)]
                contract = 'Return {"command":string|null,"report":string|null}. Unknown exact values must be null.'
                answer = {"command": "./drills/shipping-rollback.sh", "report": "artifacts/shipping-rehearsal.json"}
            candidates = negatives[:size - 2]
            positions = [size // 3, size - 1]
            candidates.insert(positions[0], relevant[0])
            candidates.insert(positions[1], relevant[1])
            assert len(candidates) == size and len(set(candidates)) == size
            assert len(user) <= 200 and all(len(s) <= 150 for s in candidates)
            cases.append({"id": f"scale-{domain}-{size:03d}", "category": f"scale_{size}",
                          "user_message": user, "candidates": candidates, "expected": positions,
                          "rationale": "Exactly two production-scope facts answer the task. Other services, environments and workflows are hard negatives. Useful count stays fixed as distractors grow.",
                          "output_contract": contract, "expected_answer": answer})
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    with args.output.open("x") as stream:
        json.dump(build_cases(), stream, ensure_ascii=False, indent=2)
        stream.write("\n")


if __name__ == "__main__":
    main()
