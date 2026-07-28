# compose-bench cross-model report

## Per-model summary (compose arm vs granular arm, means across scenarios)

| model                         | scenarios | compose adoption | Δ wall | Δ model reqs | Δ tokens | Δ cost | acc granular | acc compose |
| ----------------------------- | --------- | ---------------- | ------ | ------------ | -------- | ------ | ------------ | ----------- |
| anthropic/claude-haiku-4.5    | 6         | 6/6              | +83%   | +60%         | +172%    | +118%  | 0.83         | 1.00        |
| anthropic/claude-sonnet-4.6   | 6         | 5/6              | -35%   | -36%         | -33%     | -58%   | 0.94         | 1.00        |
| deepseek/deepseek-v4-pro      | 6         | 5/6              | +48%   | +3%          | +29%     | +19%   | 1.00         | 0.97        |
| google/gemini-3.1-pro-preview | 6         | 6/6              | +143%  | -21%         | +7%      | +55%   | 0.86         | 1.00        |
| moonshotai/kimi-k2.6          | 4         | 4/4              | -32%   | -43%         | -44%     | -50%   | 1.00         | 0.92        |
| openai/gpt-5-mini             | 6         | 6/6              | +61%   | -33%         | +76%     | +53%   | 1.00         | 0.85        |
| openai/gpt-5.2                | 6         | 6/6              | +38%   | -27%         | +20%     | +75%   | 1.00         | 0.90        |
| openai/gpt-5.4                | 6         | 6/6              | +24%   | -28%         | +29%     | +17%   | 0.57         | 0.80        |
| openai/gpt-5.5                | 6         | 6/6              | -25%   | -36%         | -46%     | -19%   | 1.00         | 1.00        |
| z-ai/glm-5                    | 6         | 5/6              | +6%    | +16%         | +62%     | +13%   | 0.94         | 1.00        |

## All cells

| model                         | scenario            | arm      | wall s | reqs | tool calls (compose) | tokens | cost $ | accuracy | failure                                                      |
| ----------------------------- | ------------------- | -------- | ------ | ---- | -------------------- | ------ | ------ | -------- | ------------------------------------------------------------ |
| anthropic/claude-haiku-4.5    | calendar-scheduling | compose  | 74.0   | 13   | 12 (11)              | 124063 | 0.1741 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | calendar-scheduling | granular | 19.3   | 4    | 22 (0)               | 15106  | 0.0301 | 0.00     |                                                              |
| anthropic/claude-haiku-4.5    | config-migration    | bash     | 80.8   | 40   | 39 (0)               | 224188 | 0.2487 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | config-migration    | compose  | 79.3   | 18   | 17 (16)              | 155651 | 0.1930 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | config-migration    | granular | 20.4   | 6    | 27 (0)               | 30268  | 0.0432 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | crm-hygiene         | compose  | 10.0   | 4    | 4 (1)                | 11307  | 0.0153 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | crm-hygiene         | granular | 15.6   | 5    | 24 (0)               | 17614  | 0.0265 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | log-incident        | compose  | 17.1   | 6    | 5 (4)                | 20393  | 0.0263 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | log-incident        | granular | 12.5   | 5    | 9 (0)                | 16093  | 0.0215 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | revenue-report      | compose  | 15.9   | 5    | 4 (3)                | 20047  | 0.0295 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | revenue-report      | granular | 22.8   | 6    | 63 (0)               | 31229  | 0.0474 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | support-triage      | compose  | 8.0    | 3    | 2 (1)                | 7727   | 0.0108 | 1.00     |                                                              |
| anthropic/claude-haiku-4.5    | support-triage      | granular | 14.7   | 6    | 20 (0)               | 20108  | 0.0279 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | calendar-scheduling | compose  | 19.8   | 3    | 2 (1)                | 9841   | 0.0298 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | calendar-scheduling | granular | 33.8   | 4    | 22 (0)               | 12049  | 0.0665 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | config-migration    | bash     | 29.3   | 5    | 5 (0)                | 18732  | 0.0587 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | config-migration    | compose  | 23.6   | 3    | 2 (1)                | 13920  | 0.0327 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | config-migration    | granular | 47.8   | 8    | 29 (0)               | 64600  | 0.1240 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | crm-hygiene         | compose  | 21.3   | 3    | 2 (1)                | 10537  | 0.0295 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | crm-hygiene         | granular | 38.9   | 5    | 24 (0)               | 17862  | 0.0878 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | log-incident        | compose  | 38.6   | 5    | 9 (0)                | 25688  | 0.0510 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | log-incident        | granular | 28.4   | 5    | 9 (0)                | 17237  | 0.0511 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | revenue-report      | compose  | 16.4   | 3    | 2 (1)                | 9852   | 0.0233 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | revenue-report      | granular | 46.6   | 5    | 63 (0)               | 26834  | 0.1366 | 0.67     |                                                              |
| anthropic/claude-sonnet-4.6   | support-triage      | compose  | 19.3   | 3    | 2 (1)                | 10882  | 0.0283 | 1.00     |                                                              |
| anthropic/claude-sonnet-4.6   | support-triage      | granular | 35.4   | 6    | 20 (0)               | 21046  | 0.0981 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | calendar-scheduling | compose  | 107.1  | 5    | 4 (2)                | 20318  | 0.0226 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | calendar-scheduling | granular | 71.2   | 4    | 22 (0)               | 15195  | 0.0240 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | config-migration    | bash     | 80.8   | 8    | 7 (0)                | 35655  | 0.0385 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | config-migration    | compose  | 431.0  | 10   | 12 (5)               | 96810  | 0.0947 | 0.85     |                                                              |
| deepseek/deepseek-v4-pro      | config-migration    | granular | 151.1  | 8    | 34 (0)               | 64412  | 0.0734 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | crm-hygiene         | compose  | 161.7  | 7    | 9 (1)                | 51760  | 0.0814 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | crm-hygiene         | granular | 63.8   | 5    | 24 (0)               | 21591  | 0.0289 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | log-incident        | compose  | 45.8   | 5    | 5 (1)                | 23839  | 0.0159 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | log-incident        | granular | 56.2   | 5    | 9 (0)                | 18055  | 0.0165 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | revenue-report      | compose  | 27.6   | 4    | 3 (1)                | 11131  | 0.0085 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | revenue-report      | granular | 135.2  | 10   | 63 (0)               | 60197  | 0.0641 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | support-triage      | compose  | 73.4   | 6    | 20 (0)               | 31391  | 0.0284 | 1.00     |                                                              |
| deepseek/deepseek-v4-pro      | support-triage      | granular | 73.6   | 7    | 21 (0)               | 31209  | 0.0284 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | calendar-scheduling | compose  | 19.8   | 3    | 2 (1)                | 6019   | 0.0308 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | calendar-scheduling | granular | 30.3   | 6    | 14 (0)               | 8082   | 0.0403 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | config-migration    | bash     | 24.9   | 5    | 4 (0)                | 7789   | 0.0347 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | config-migration    | compose  | 47.6   | 5    | 4 (3)                | 18277  | 0.0892 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | config-migration    | granular | 41.3   | 5    | 26 (0)               | 16712  | 0.0804 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | crm-hygiene         | compose  | 47.1   | 3    | 2 (1)                | 10591  | 0.0705 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | crm-hygiene         | granular | 9.0    | 2    | 9 (0)                | 4230   | 0.0156 | 0.17     |                                                              |
| google/gemini-3.1-pro-preview | log-incident        | compose  | 21.5   | 4    | 3 (2)                | 9741   | 0.0349 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | log-incident        | granular | 27.2   | 6    | 13 (0)               | 10726  | 0.0382 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | revenue-report      | compose  | 15.6   | 3    | 2 (1)                | 4718   | 0.0219 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | revenue-report      | granular | 32.9   | 5    | 63 (0)               | 14940  | 0.0643 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | support-triage      | compose  | 135.3  | 3    | 2 (1)                | 9435   | 0.0576 | 1.00     |                                                              |
| google/gemini-3.1-pro-preview | support-triage      | granular | 21.6   | 6    | 20 (0)               | 10834  | 0.0355 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | calendar-scheduling | compose  | 600.0  | 4    | 4 (4)                | 20814  | 0.0275 | 0.00     | timed out after 600s                                         |
| moonshotai/kimi-k2.6          | calendar-scheduling | granular | 171.7  | 7    | 15 (0)               | 18077  | 0.0225 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | config-migration    | bash     | 358.7  | 15   | 14 (0)               | 43902  | 0.0387 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | config-migration    | compose  | 228.2  | 7    | 6 (3)                | 62052  | 0.0291 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | config-migration    | granular | 600.0  | 4    | 19 (0)               | 27813  | 0.0367 | 0.41     | timed out after 600s                                         |
| moonshotai/kimi-k2.6          | crm-hygiene         | compose  | 187.7  | 3    | 2 (1)                | 14222  | 0.0134 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | crm-hygiene         | granular | 231.6  | 7    | 24 (0)               | 31391  | 0.0308 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | log-incident        | compose  | 68.2   | 4    | 4 (1)                | 14149  | 0.0092 | 0.67     |                                                              |
| moonshotai/kimi-k2.6          | log-incident        | granular | 119.6  | 7    | 7 (0)                | 16811  | 0.0155 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | revenue-report      | compose  | 199.2  | 5    | 4 (2)                | 15430  | 0.0161 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | revenue-report      | granular | 285.2  | 6    | 63 (0)               | 26908  | 0.0334 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | support-triage      | compose  | 114.0  | 4    | 3 (2)                | 11371  | 0.0140 | 1.00     |                                                              |
| moonshotai/kimi-k2.6          | support-triage      | granular | 181.3  | 9    | 28 (0)               | 31652  | 0.0289 | 1.00     |                                                              |
| openai/gpt-5-mini             | calendar-scheduling | compose  | 140.6  | 8    | 26 (4)               | 78336  | 0.0222 | 1.00     |                                                              |
| openai/gpt-5-mini             | calendar-scheduling | granular | 40.7   | 4    | 22 (0)               | 9544   | 0.0063 | 1.00     |                                                              |
| openai/gpt-5-mini             | config-migration    | bash     | 73.9   | 4    | 3 (0)                | 13353  | 0.0120 | 1.00     |                                                              |
| openai/gpt-5-mini             | config-migration    | compose  | 55.1   | 3    | 2 (1)                | 13983  | 0.0103 | 0.42     |                                                              |
| openai/gpt-5-mini             | config-migration    | granular | 138.5  | 27   | 26 (0)               | 180735 | 0.0178 | 1.00     |                                                              |
| openai/gpt-5-mini             | crm-hygiene         | compose  | 26.6   | 2    | 1 (1)                | 4888   | 0.0047 | 1.00     |                                                              |
| openai/gpt-5-mini             | crm-hygiene         | granular | 81.1   | 7    | 24 (0)               | 24244  | 0.0123 | 1.00     |                                                              |
| openai/gpt-5-mini             | log-incident        | compose  | 217.4  | 8    | 8 (8)                | 102716 | 0.0288 | 0.00     | loop error: provider error: failed to read OpenRouter respon |
| openai/gpt-5-mini             | log-incident        | compose  | 66.7   | 4    | 3 (2)                | 23964  | 0.0110 | 0.67     |                                                              |
| openai/gpt-5-mini             | log-incident        | granular | 14.7   | 6    | 8 (0)                | 16578  | 0.0028 | 1.00     |                                                              |
| openai/gpt-5-mini             | revenue-report      | compose  | 20.8   | 3    | 2 (1)                | 7055   | 0.0030 | 1.00     |                                                              |
| openai/gpt-5-mini             | revenue-report      | granular | 58.1   | 7    | 63 (0)               | 20618  | 0.0106 | 1.00     |                                                              |
| openai/gpt-5-mini             | support-triage      | compose  | 26.8   | 3    | 2 (1)                | 6228   | 0.0039 | 1.00     |                                                              |
| openai/gpt-5-mini             | support-triage      | granular | 44.7   | 6    | 20 (0)               | 20466  | 0.0078 | 1.00     |                                                              |
| openai/gpt-5.2                | calendar-scheduling | compose  | 30.3   | 3    | 2 (2)                | 8427   | 0.0319 | 1.00     |                                                              |
| openai/gpt-5.2                | calendar-scheduling | granular | 29.2   | 4    | 22 (0)               | 8377   | 0.0276 | 1.00     |                                                              |
| openai/gpt-5.2                | config-migration    | bash     | 94.2   | 7    | 6 (0)                | 16605  | 0.0296 | 1.00     |                                                              |
| openai/gpt-5.2                | config-migration    | compose  | 22.7   | 3    | 2 (1)                | 8523   | 0.0204 | 0.42     |                                                              |
| openai/gpt-5.2                | config-migration    | granular | 58.4   | 6    | 26 (0)               | 33621  | 0.0614 | 1.00     |                                                              |
| openai/gpt-5.2                | crm-hygiene         | compose  | 34.4   | 3    | 2 (1)                | 8924   | 0.0323 | 1.00     |                                                              |
| openai/gpt-5.2                | crm-hygiene         | granular | 38.6   | 5    | 24 (0)               | 17108  | 0.0431 | 1.00     |                                                              |
| openai/gpt-5.2                | log-incident        | compose  | 143.0  | 9    | 8 (6)                | 103187 | 0.1476 | 1.00     |                                                              |
| openai/gpt-5.2                | log-incident        | granular | 26.7   | 6    | 9 (0)                | 21492  | 0.0205 | 1.00     |                                                              |
| openai/gpt-5.2                | revenue-report      | compose  | 7.2    | 3    | 2 (1)                | 4880   | 0.0090 | 1.00     |                                                              |
| openai/gpt-5.2                | revenue-report      | granular | 43.9   | 6    | 63 (0)               | 28418  | 0.0549 | 1.00     |                                                              |
| openai/gpt-5.2                | support-triage      | compose  | 20.9   | 4    | 3 (2)                | 11297  | 0.0242 | 1.00     |                                                              |
| openai/gpt-5.2                | support-triage      | granular | 46.2   | 8    | 20 (0)               | 25980  | 0.0276 | 1.00     |                                                              |
| openai/gpt-5.4                | calendar-scheduling | compose  | 17.8   | 4    | 3 (2)                | 10267  | 0.0166 | 1.00     |                                                              |
| openai/gpt-5.4                | calendar-scheduling | granular | 6.4    | 4    | 6 (0)                | 2387   | 0.0083 | 0.00     |                                                              |
| openai/gpt-5.4                | config-migration    | bash     | 17.6   | 7    | 6 (0)                | 10067  | 0.0220 | 1.00     |                                                              |
| openai/gpt-5.4                | config-migration    | compose  | 6.5    | 3    | 2 (1)                | 7500   | 0.0092 | 0.12     |                                                              |
| openai/gpt-5.4                | config-migration    | granular | 20.7   | 5    | 24 (0)               | 19275  | 0.0292 | 0.73     |                                                              |
| openai/gpt-5.4                | crm-hygiene         | compose  | 10.1   | 3    | 2 (1)                | 4442   | 0.0186 | 1.00     |                                                              |
| openai/gpt-5.4                | crm-hygiene         | granular | 11.5   | 5    | 24 (0)               | 12391  | 0.0185 | 0.83     |                                                              |
| openai/gpt-5.4                | log-incident        | compose  | 25.7   | 5    | 4 (3)                | 20727  | 0.0317 | 0.67     |                                                              |
| openai/gpt-5.4                | log-incident        | granular | 13.9   | 5    | 8 (0)                | 11692  | 0.0147 | 1.00     |                                                              |
| openai/gpt-5.4                | revenue-report      | compose  | 5.8    | 3    | 2 (1)                | 5580   | 0.0079 | 1.00     |                                                              |
| openai/gpt-5.4                | revenue-report      | granular | 26.2   | 7    | 63 (0)               | 27768  | 0.0324 | 0.00     |                                                              |
| openai/gpt-5.4                | support-triage      | compose  | 18.7   | 4    | 3 (2)                | 9831   | 0.0212 | 1.00     |                                                              |
| openai/gpt-5.4                | support-triage      | granular | 13.4   | 6    | 17 (0)               | 14239  | 0.0162 | 0.86     |                                                              |
| openai/gpt-5.5                | calendar-scheduling | compose  | 24.6   | 3    | 2 (1)                | 5720   | 0.0389 | 1.00     |                                                              |
| openai/gpt-5.5                | calendar-scheduling | granular | 17.7   | 4    | 22 (0)               | 6938   | 0.0427 | 1.00     |                                                              |
| openai/gpt-5.5                | config-migration    | bash     | 64.3   | 8    | 7 (0)                | 22943  | 0.0932 | 1.00     |                                                              |
| openai/gpt-5.5                | config-migration    | compose  | 22.7   | 4    | 3 (2)                | 13243  | 0.0478 | 1.00     |                                                              |
| openai/gpt-5.5                | config-migration    | granular | 42.0   | 7    | 30 (0)               | 35568  | 0.1131 | 1.00     |                                                              |
| openai/gpt-5.5                | crm-hygiene         | compose  | 19.7   | 2    | 1 (1)                | 3601   | 0.0433 | 1.00     |                                                              |
| openai/gpt-5.5                | crm-hygiene         | granular | 24.2   | 4    | 25 (0)               | 11564  | 0.0677 | 1.00     |                                                              |
| openai/gpt-5.5                | log-incident        | compose  | 30.6   | 5    | 4 (3)                | 16396  | 0.0741 | 1.00     |                                                              |
| openai/gpt-5.5                | log-incident        | granular | 30.5   | 5    | 8 (0)                | 12837  | 0.0375 | 1.00     |                                                              |
| openai/gpt-5.5                | revenue-report      | compose  | 9.3    | 2    | 1 (1)                | 2505   | 0.0239 | 1.00     |                                                              |
| openai/gpt-5.5                | revenue-report      | granular | 44.5   | 4    | 69 (0)               | 19571  | 0.1044 | 1.00     |                                                              |
| openai/gpt-5.5                | support-triage      | compose  | 15.7   | 3    | 2 (1)                | 5568   | 0.0361 | 1.00     |                                                              |
| openai/gpt-5.5                | support-triage      | granular | 27.7   | 6    | 20 (0)               | 17216  | 0.0531 | 1.00     |                                                              |
| z-ai/glm-5                    | calendar-scheduling | compose  | 76.0   | 7    | 6 (5)                | 55068  | 0.0220 | 1.00     |                                                              |
| z-ai/glm-5                    | calendar-scheduling | granular | 32.1   | 4    | 22 (0)               | 14205  | 0.0099 | 1.00     |                                                              |
| z-ai/glm-5                    | config-migration    | bash     | 54.1   | 10   | 38 (0)               | 68753  | 0.0178 | 1.00     |                                                              |
| z-ai/glm-5                    | config-migration    | compose  | 59.8   | 8    | 7 (6)                | 70911  | 0.0186 | 1.00     |                                                              |
| z-ai/glm-5                    | config-migration    | granular | 48.9   | 5    | 26 (0)               | 32919  | 0.0144 | 1.00     |                                                              |
| z-ai/glm-5                    | crm-hygiene         | compose  | 43.3   | 6    | 5 (2)                | 24665  | 0.0103 | 1.00     |                                                              |
| z-ai/glm-5                    | crm-hygiene         | granular | 61.2   | 5    | 24 (0)               | 24719  | 0.0127 | 1.00     |                                                              |
| z-ai/glm-5                    | log-incident        | compose  | 25.2   | 4    | 5 (0)                | 16103  | 0.0062 | 1.00     |                                                              |
| z-ai/glm-5                    | log-incident        | granular | 37.6   | 6    | 8 (0)                | 21548  | 0.0082 | 1.00     |                                                              |
| z-ai/glm-5                    | revenue-report      | compose  | 17.1   | 3    | 2 (1)                | 8009   | 0.0032 | 1.00     |                                                              |
| z-ai/glm-5                    | revenue-report      | granular | 98.9   | 7    | 63 (0)               | 45499  | 0.0218 | 0.67     |                                                              |
| z-ai/glm-5                    | support-triage      | compose  | 66.5   | 8    | 21 (1)               | 47431  | 0.0192 | 1.00     |                                                              |
| z-ai/glm-5                    | support-triage      | granular | 55.6   | 6    | 20 (0)               | 26791  | 0.0122 | 1.00     |                                                              |

Total runs: 131; total reported cost: $5.05
