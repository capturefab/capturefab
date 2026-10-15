# Models for automatic capture

Research date: October 14, 2026. The recommendation below is an engineering
judgment for interactive camera capture, not a claim that one model wins on every
camera or defect type. Published industrial benchmark scores do not measure a
particular user's capture preferences or desktop CPU latency.

## What the model must learn

The primary task is **learning which frames the user wants to capture**. A manual
capture and a kept suggestion are positive examples; a discarded suggestion is a
negative example. This is different from industrial anomaly detection, where
training images usually show normal products and the model flags departures from
normality. A rare scene can be uninteresting, and a useful capture can resemble
many previously seen frames.

Use a small, session-local preference model that updates as soon as feedback is
saved. Keep novelty, user preference, and temporal duplicate suppression as
distinct decisions. Never treat an unreviewed auto capture as a positive example:
that would let the model reinforce its own mistakes. A discarded frame means
"do not capture frames like this"; it does not necessarily mean "normal product."

## Shortlist and recommendation

| Approach | Best use here | Adaptation | Main tradeoff |
| --- | --- | --- | --- |
| Frozen DINOv2 small encoder + nearest labeled examples | First pretrained option to evaluate for general capture preference | Add or relabel an embedding immediately; no encoder retraining | Weight/runtime dependency; target-device speed and camera-domain accuracy need measurement |
| Frozen DINOv3 small encoder + nearest labeled examples | Newer alternative when dense spatial features help | Same online preference head | Custom model license, access step, and deployment validation |
| AnomalyDINO | Few-normal-shot inspection without neural fine-tuning | DINOv2 patch-feature nearest-neighbor reference bank | Normal references and patch search remain different from wanted-capture examples |
| PatchCore | Known inspection setup with representative normal examples and small local defects | Build/update a normal patch-feature memory bank | Its normal-only objective does not directly learn keep/discard preferences; patch memory can grow |
| EfficientAD | Stable, repeated inspection task that can afford task training | Train student/autoencoder on normal images, then infer | Per-task optimization is less convenient for a session that begins with a few captures |
| WinCLIP / WinCLIP+ | Optional text-defined or few-normal-shot defect detection | Text prompts, plus normal reference images | Prompt meaning and the user's capture intent need alignment |
| Native visual descriptor + nearest labeled examples | Immediate offline CPU baseline with no model download | Add or relabel small descriptors | Limited semantic understanding and sensitivity to lighting, alignment, and scale |

**Recommendation:** use a session-local nearest-example preference head now, with
the native descriptor as an explicit lightweight baseline. The first pretrained
upgrade to benchmark is DINOv2 ViT-S/14; compare it against DINOv3 ViT-S/16 on the
actual capture task before choosing a default. For inspection-specific anomaly
localization, evaluate a separate PatchCore branch with explicitly collected
normal references; include AnomalyDINO as the more recent few-shot comparison. Do
not feed wanted defect captures into a normal-only memory bank.

### DINOv2 and DINOv3: reusable representations

DINOv2's authors provide frozen visual features, k-nearest-neighbor and linear
classification evaluation, and a 21-million-parameter ViT-S/14. The standard
DINOv2 code and weights use Apache 2.0; specialized biological/X-ray releases
have separate terms. These properties make the standard small model a practical
candidate for a lightweight preference head, but its ImageNet results do not
establish anomaly-capture performance.
[DINOv2 paper](https://arxiv.org/abs/2304.07193),
[official models and evaluation code](https://github.com/facebookresearch/dinov2).

DINOv3 emphasizes dense visual features and provides a distilled
21-million-parameter ViT-S/16. The official distribution requires a weight-access
step and uses the DINOv3 license. It is a strong candidate to compare, especially
for spatial detail; assuming it is automatically best for small industrial
defects or CPU capture would go beyond the evidence.
[DINOv3 paper](https://arxiv.org/abs/2508.10104),
[official model family and access instructions](https://github.com/facebookresearch/dinov3).

The proposed use of either encoder with online keep/discard examples is an
application design inference, not an implemented DINO backend or a performance
claim from those papers. A neural encoder would stay frozen; only the session's
labeled example bank or small classifier would change. Encoder identity,
preprocessing version, and embedding dimensions must be stored with its data so
that incompatible features are never mixed on resume.

### AnomalyDINO: a directly relevant few-shot comparator

AnomalyDINO uses frozen DINOv2 patch features and nearest-neighbor comparison to
normal reference images, without neural fine-tuning. The WACV 2025 paper reports
96.6% one-shot image-level AUROC on MVTec AD. Its official code supports CPU
similarity search; this does not by itself establish CPU encoder throughput.
This is a particularly useful pretrained comparison for a few-normal-example
inspection mode. A session that instead learns wanted captures still needs
positive/negative preference labels and an appropriate decision head.
[AnomalyDINO paper](https://openaccess.thecvf.com/content/WACV2025/papers/Damm_AnomalyDINO_Boosting_Patch-Based_Few-Shot_Anomaly_Detection_with_DINOv2_WACV_2025_paper.pdf),
[official implementation](https://github.com/dammsi/AnomalyDINO).

### PatchCore: normal-only local defect detection

PatchCore stores representative pretrained patch features from nominal images
and scores distance from that memory. The paper reports up to 99.6% image-level
AUROC on MVTec AD and includes a few-sample study. This supports evaluating it
for texture/structural defects when normal references exist. It does not imply
99.6% useful-capture accuracy, nor does it establish CPU streaming throughput.
Keep/discard preference learning requires an additional head or decision policy.
[PatchCore paper](https://openaccess.thecvf.com/content/CVPR2022/html/Roth_Towards_Total_Recall_in_Industrial_Anomaly_Detection_CVPR_2022_paper.html),
[author implementation](https://github.com/amazon-science/patchcore-inspection).

### EfficientAD: efficient inference after task training

EfficientAD combines a student-teacher discrepancy model with an autoencoder for
logical anomalies. It trains on normal images. Its reported approximately
2-millisecond latency and more than 600 images/second are GPU experimental
results, not a guarantee for desktop CPUs or this application. It is attractive
for a recurring inspection station; updating it after each swipe is more work
than updating a frozen-feature example bank.
[EfficientAD paper](https://openaccess.thecvf.com/content/WACV2024/papers/Batzner_EfficientAD_Accurate_Visual_Anomaly_Detection_at_Millisecond-Level_Latencies_WACV_2024_paper.pdf).

### WinCLIP: language or a few normal references

WinCLIP uses text prompts and features at multiple spatial scales; WinCLIP+ adds
normal reference images. The authors evaluate zero-shot and one-to-few-normal-shot
industrial anomaly detection. This is useful if users can describe wanted events
or define normality, but it adds a different input workflow and should not replace
learning from the actual captures and review decisions.
[WinCLIP paper](https://openaccess.thecvf.com/content/CVPR2023/html/Jeong_WinCLIP_Zero-Few-Shot_Anomaly_Classification_and_Segmentation_CVPR_2023_paper.html).

## Implemented baseline and its limits

The initial automatic capture implementation uses native image descriptors and a
session-local nearest-example model. It learns nonparametrically by saving manual
capture examples and review labels; it does not train a neural network, run
PatchCore or EfficientAD, download DINO weights, or require a cloud service.

This baseline makes feedback immediate and keeps the feature usable offline.
It analyzes a preview of at most 64 × 64 pixels at up to four samples per second,
with bounded banks of 256 positive and 256 negative examples and 64 background
prototypes. Events shorter than the sampling interval and tiny image defects
can be missed. The saved captures retain the original sensor resolution and
sample bytes; the small preview is only the learning representation.
It is most useful with a reasonably stable camera view and visually consistent
capture targets. It can miss small defects, confuse semantically different but
visually similar frames, and react to illumination or viewpoint changes. A model
score expresses a relative decision signal, not a calibrated probability. Human
review remains the source of truth.

The portable session artifact is one DuckDB database containing the saved image
bytes, source/camera identity, frame metadata, descriptors, capture origin,
pending/kept/discarded decisions, model/settings version, and review history.
Resuming that file restores the examples and pending review queue. The live
camera connection must be established again; a database cannot restore access to
hardware that is unavailable.

## How to compare a future encoder

Evaluate on recorded streams from the intended cameras, with human capture and
keep/discard labels. Split by recording/session or time block, rather than by
adjacent frames, so nearly identical frames do not leak across evaluation sets.
Replay examples and feedback in timestamp order; never use a future review label
to score an earlier frame.

Compare the descriptor baseline, DINOv2 small, DINOv3 small, and (for a normal-only
inspection subset) PatchCore and AnomalyDINO. Measure:

- Useful-event recall and accepted suggestions per review minute, with a fixed
  suggestion budget. Also measure false captures per minute and duplicate rate.
- Cold-start behavior after 1, 3, 5, and 10 manual examples; improvement after
  positive and negative review feedback; behavior when the user changes targets.
- End-to-end CPU p50/p95 latency, memory, power, preview frame rate, and backlog
  under simultaneous cameras. Published GPU numbers are insufficient.
- Exposure/white-balance drift, vibration, new backgrounds, rare small defects,
  grayscale/Bayer inputs, camera changes, and unsupported/malformed frame data.
- Identical decisions after database close/reopen, file transfer, feedback
  correction, and interrupted writes; source isolation across multiple cameras.

Use human labels from a representative sample of unsuggested frames as well as
the review queue. Reviewing only suggestions cannot reveal missed events and
creates selection bias. Promote a larger encoder only when the useful-capture
gain justifies its measured latency, storage, download, and packaging costs.
