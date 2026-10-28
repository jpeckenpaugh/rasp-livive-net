/*
 * RASP listening harness — HR-2 precursor to U-12.
 *
 * One blind trial at a time: A (original) and B (processed) are played through
 * plain <audio> elements and neither is labelled. The preset name is carried in
 * the pack as base64 so that it never appears in the page, in the DOM, or in a
 * file name while the round is running; it is decoded only when the CSV is
 * exported and on the end-of-round summary.
 *
 * Data source, in order:
 *   1. inline JSON written by scripts/make_hr2_pack.py (works from file://);
 *   2. ?data=<url> (HTTP only);
 *   3. ./qa-data.json (HTTP only).
 *
 * No network, no storage beyond localStorage, no third-party code.
 */
(function () {
  "use strict";

  var VERSION = "rasp-hr2/1";
  var SCHEMA = "rasp-hr2-pack/1";
  var QUESTIONS = ["q1", "q2", "q3", "q4", "q5"];
  var STORE_PREFIX = "rasp-hr2:v1:";

  var DATA = null;
  var STATE = null;
  var STORE_KEY = null;
  var AUDIO_BASE = null; // null => audio paths are relative to this page
  var sequence = 0;
  var els = {};

  function byId(id) {
    return document.getElementById(id);
  }

  // -------------------------------------------------------------------------
  // Boot
  // -------------------------------------------------------------------------

  function boot() {
    els = {
      boot: byId("boot"),
      fatal: byId("fatal"),
      fatalMessage: byId("fatal-message"),
      app: byId("app"),
      trialView: byId("trial-view"),
      summaryView: byId("summary-view"),
      summaryTitle: byId("summary-title"),
      summaryIntro: byId("summary-intro"),
      summaryBody: byId("summary-body"),
      summaryStatus: byId("summary-status"),
      csvFallback: byId("csv-fallback"),
      trialHeading: byId("trial-heading"),
      progressBar: document.querySelector(".bar[role='progressbar']"),
      barFill: byId("bar-fill"),
      playA: byId("play-a"),
      playB: byId("play-b"),
      replay: byId("replay"),
      audioStatus: byId("audio-status"),
      form: byId("questions"),
      notes: byId("notes"),
      next: byId("next"),
      nextHint: byId("next-hint"),
      exportButton: byId("export"),
      exportStatus: byId("export-status"),
      download: byId("download"),
      restart: byId("restart"),
      audioA: byId("audio-a"),
      audioB: byId("audio-b"),
      seedLine: byId("seed-line")
    };

    loadData()
      .then(function (result) {
        if (!result || !result.data) {
          showFatal(
            "No round data was found on this page. Open the index.html that " +
              "scripts/make_hr2_pack.py generated, or serve a folder containing " +
              "qa-data.json."
          );
          return;
        }
        AUDIO_BASE = result.audioBase;
        acceptData(result.data);
      })
      .catch(function (err) {
        showFatal("The round data could not be read: " + String(err && err.message ? err.message : err));
      });
  }

  function showFatal(message) {
    els.boot.hidden = true;
    els.fatal.hidden = false;
    els.fatalMessage.textContent = message;
  }

  function inlineData() {
    var tag = byId("qa-data");
    if (!tag) return null;
    var text = (tag.textContent || "").trim();
    if (!text || text === "null") return null;
    return JSON.parse(text);
  }

  function fetchFirst(urls) {
    var i = 0;
    function step() {
      if (i >= urls.length) return Promise.resolve(null);
      var url = urls[i++];
      return fetch(url, { cache: "no-store" }).then(function (response) {
        if (!response.ok) return step();
        return response.json().then(function (data) {
          return { data: data, url: response.url || url };
        }).catch(function () {
          return step();
        });
      }).catch(function () {
        return step();
      });
    }
    return step();
  }

  function loadData() {
    var inline = inlineData();
    if (inline) return Promise.resolve({ data: inline, audioBase: null });

    var urls = [];
    try {
      var wanted = new URLSearchParams(window.location.search).get("data");
      if (wanted) urls.push(wanted);
    } catch (err) {
      /* URLSearchParams is not available in very old browsers; fall through. */
    }
    urls.push("qa-data.json");
    return fetchFirst(urls).then(function (found) {
      if (!found) return null;
      // Audio lives beside the data file, which is not always beside this page.
      var dataDir;
      try {
        dataDir = new URL(".", new URL(found.url, document.baseURI)).href;
      } catch (err) {
        dataDir = null;
      }
      var pageDir = new URL(".", document.baseURI).href;
      return { data: found.data, audioBase: dataDir === pageDir ? null : dataDir };
    });
  }

  function acceptData(data) {
    if (data.schema !== SCHEMA) {
      showFatal("This page does not understand the pack format (" + String(data.schema) + "). Rebuild the pack.");
      return;
    }
    if (!Array.isArray(data.trials) || data.trials.length === 0) {
      showFatal("This pack contains no trials.");
      return;
    }
    DATA = data;
    STORE_KEY = STORE_PREFIX + data.seed + ":" + data.orderSha256;
    var saved = readState();
    STATE = saved || freshState();
    var hadProgress = Boolean(saved && saved.answers && Object.keys(saved.answers).length);

    els.boot.hidden = true;
    els.app.hidden = false;
    els.seedLine.textContent = "seed " + data.seed;

    wireAudio();
    wireEvents();

    var firstOpen = firstIncompleteIndex();
    if (firstOpen === -1) {
      STATE.index = DATA.trials.length - 1;
      showSummary();
    } else {
      STATE.index = firstOpen;
      renderTrial();
      if (hadProgress) {
        setAudioStatus("Resumed at trial " + (STATE.index + 1) + " — earlier answers were restored.");
      }
    }

    window.__RASP_QA__ = {
      version: VERSION,
      data: DATA,
      state: function () {
        return STATE;
      },
      csv: buildCsv,
      presetOf: presetOf
    };
  }

  // -------------------------------------------------------------------------
  // State
  // -------------------------------------------------------------------------

  function freshState() {
    return {
      version: 1,
      seed: DATA.seed,
      orderSha256: DATA.orderSha256,
      index: 0,
      answers: {},
      updatedUtc: null
    };
  }

  function readState() {
    try {
      var raw = window.localStorage.getItem(STORE_KEY);
      if (!raw) return null;
      var stored = JSON.parse(raw);
      if (!stored || stored.version !== 1 || stored.orderSha256 !== DATA.orderSha256) return null;
      if (!stored.answers || typeof stored.answers !== "object") return null;
      return stored;
    } catch (err) {
      return null;
    }
  }

  function saveState() {
    STATE.updatedUtc = nowIso();
    try {
      window.localStorage.setItem(STORE_KEY, JSON.stringify(STATE));
    } catch (err) {
      /* Private mode or a full quota: the round still works, just without resume. */
    }
  }

  function answersFor(trialId) {
    if (!STATE.answers[trialId]) {
      STATE.answers[trialId] = { q1: null, q2: null, q3: null, q4: null, q5: null, notes: "", completedUtc: "" };
    }
    return STATE.answers[trialId];
  }

  function isComplete(answers) {
    return QUESTIONS.every(function (q) {
      var value = answers[q];
      return typeof value === "number" && value >= 1;
    });
  }

  function currentTrial() {
    return DATA.trials[STATE.index];
  }

  function firstIncompleteIndex() {
    for (var i = 0; i < DATA.trials.length; i += 1) {
      var answers = STATE.answers[DATA.trials[i].trial];
      if (!answers || !isComplete(answers)) return i;
    }
    return -1;
  }

  // -------------------------------------------------------------------------
  // Trial rendering
  // -------------------------------------------------------------------------

  function renderTrial() {
    var trial = currentTrial();
    var answers = answersFor(trial.trial);
    var total = DATA.trials.length;
    var shown = STATE.index + 1;

    els.trialView.hidden = false;
    els.summaryView.hidden = true;

    els.trialHeading.textContent = "Trial " + shown + " of " + total;
    els.progressBar.setAttribute("aria-valuemax", String(total));
    els.progressBar.setAttribute("aria-valuenow", String(shown));
    els.barFill.style.width = ((shown - 1) / total) * 100 + "%";

    els.audioA.src = audioSource(trial.a);
    els.audioB.src = audioSource(trial.b);
    els.audioA.load();
    els.audioB.load();

    QUESTIONS.forEach(function (q) {
      var inputs = els.form.querySelectorAll("input[name='" + q + "']");
      for (var i = 0; i < inputs.length; i += 1) {
        inputs[i].checked = answers[q] === Number(inputs[i].value);
      }
    });
    els.notes.value = answers.notes || "";

    els.next.textContent = shown === total ? "Finish" : "Next";
    setAudioStatus("");
    updateGate();
    window.scrollTo(0, 0);
    els.trialHeading.focus();
  }

  function updateGate() {
    var answers = answersFor(currentTrial().trial);
    var complete = isComplete(answers);
    var missing = QUESTIONS.filter(function (q) {
      return !(typeof answers[q] === "number" && answers[q] >= 1);
    }).length;

    els.next.disabled = !complete;
    if (complete) {
      els.nextHint.textContent =
        STATE.index + 1 === DATA.trials.length ? "Press → or click Finish" : "Press → or click Next";
    } else {
      els.nextHint.textContent =
        missing === 1 ? "1 answer left before Next" : missing + " answers left before Next";
    }

    var firstOpen = els.form.querySelector("fieldset[data-q='" + firstUnanswered() + "']");
    var all = els.form.querySelectorAll("fieldset.q");
    for (var i = 0; i < all.length; i += 1) all[i].classList.remove("focus-q");
    if (firstOpen && missing > 0) firstOpen.classList.add("focus-q");
  }

  function firstUnanswered() {
    var answers = answersFor(currentTrial().trial);
    for (var i = 0; i < QUESTIONS.length; i += 1) {
      if (!(typeof answers[QUESTIONS[i]] === "number" && answers[QUESTIONS[i]] >= 1)) return QUESTIONS[i];
    }
    return null;
  }

  function recordAnswer(question, value) {
    var answers = answersFor(currentTrial().trial);
    answers[question] = value;
    if (isComplete(answers) && !answers.completedUtc) answers.completedUtc = nowIso();
    saveState();
  }

  // -------------------------------------------------------------------------
  // Audio
  // -------------------------------------------------------------------------

  function wireAudio() {
    [
      ["a", els.audioA],
      ["b", els.audioB]
    ].forEach(function (pair) {
      pair[1].addEventListener("error", function () {
        setAudioStatus(
          "Could not load audio " + pair[0].toUpperCase() + ". A pack file may be missing.",
          true
        );
      });
    });
  }

  function stopAll() {
    sequence += 1;
    [els.audioA, els.audioB].forEach(function (el) {
      el.onended = null;
      try {
        el.pause();
        el.currentTime = 0;
      } catch (err) {
        /* Not loaded yet. */
      }
    });
  }

  function playElement(el, label) {
    try {
      el.currentTime = 0;
    } catch (err) {
      /* Ignore. */
    }
    var promise = el.play();
    if (promise && typeof promise.catch === "function") {
      promise.catch(function (err) {
        if (err && err.name === "AbortError") return;
        setAudioStatus("Could not play " + label + ". Click the button again.", true);
      });
    }
  }

  function playOne(which) {
    stopAll();
    var el = which === "a" ? els.audioA : els.audioB;
    setAudioStatus("Playing " + which.toUpperCase() + "…");
    playElement(el, which.toUpperCase());
  }

  function replayBoth() {
    stopAll();
    var token = sequence;
    var a = els.audioA;
    var b = els.audioB;
    setAudioStatus("Playing A, then B…");

    a.onended = function () {
      a.onended = null;
      if (token !== sequence) return;
      window.setTimeout(function () {
        if (token !== sequence) return;
        playElement(b, "B");
      }, 180);
    };
    playElement(a, "A");
  }

  function setAudioStatus(message, bad) {
    els.audioStatus.textContent = message || "";
    els.audioStatus.classList.toggle("bad", Boolean(bad));
  }

  // -------------------------------------------------------------------------
  // Questions and navigation
  // -------------------------------------------------------------------------

  function wireEvents() {
    els.form.addEventListener("change", function (event) {
      var input = event.target;
      if (!input || input.type !== "radio") return;
      if (QUESTIONS.indexOf(input.name) === -1) return;
      recordAnswer(input.name, Number(input.value));
      updateGate();
    });

    els.notes.addEventListener("input", function () {
      answersFor(currentTrial().trial).notes = els.notes.value.slice(0, 200);
      saveState();
    });

    els.playA.addEventListener("click", function () {
      playOne("a");
    });
    els.playB.addEventListener("click", function () {
      playOne("b");
    });
    els.replay.addEventListener("click", replayBoth);
    els.next.addEventListener("click", nextTrial);
    els.exportButton.addEventListener("click", function () {
      downloadCsv(els.exportStatus);
    });
    els.download.addEventListener("click", function () {
      downloadCsv(els.summaryStatus);
    });
    els.restart.addEventListener("click", restart);

    document.addEventListener("keydown", onKeydown);
  }

  function applyAnswer(question, value) {
    var input = els.form.querySelector("input[name='" + question + "'][value='" + value + "']");
    if (!input) return false;
    input.checked = true;
    recordAnswer(question, value);
    updateGate();
    return true;
  }

  function onKeydown(event) {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    var target = event.target;
    var tag = target && target.tagName ? target.tagName : "";
    var typing = (tag === "INPUT" && target.type === "text") || tag === "TEXTAREA" || tag === "SELECT";
    if (typing) return;

    var key = event.key;
    if (key === "a" || key === "A") {
      if (!els.trialView.hidden) playOne("a");
    } else if (key === "b" || key === "B") {
      if (!els.trialView.hidden) playOne("b");
    } else if (key === "r" || key === "R") {
      if (!els.trialView.hidden) replayBoth();
    } else if (/^[1-5]$/.test(key)) {
      if (els.trialView.hidden) return;
      var question = firstUnanswered();
      if (question) {
        event.preventDefault();
        applyAnswer(question, Number(key));
      }
    } else if (key === "ArrowRight") {
      if (!els.trialView.hidden && !els.next.disabled) {
        event.preventDefault();
        nextTrial();
      }
    }
  }

  function nextTrial() {
    if (els.next.disabled) return;
    stopAll();
    saveState();
    if (STATE.index + 1 < DATA.trials.length) {
      STATE.index += 1;
      saveState();
      renderTrial();
    } else {
      showSummary();
    }
  }

  // -------------------------------------------------------------------------
  // Summary
  // -------------------------------------------------------------------------

  function showSummary() {
    stopAll();
    els.trialView.hidden = true;
    els.summaryView.hidden = false;
    var total = DATA.trials.length;

    els.summaryIntro.textContent =
      "All " +
      total +
      " trials are answered. Preset names are shown below — they were hidden during the round. " +
      "Download the CSV to hand the results back.";

    var byPreset = {};
    var counts = {};
    DATA.trials.forEach(function (trial) {
      var answers = STATE.answers[trial.trial];
      if (!answers || !isComplete(answers)) return;
      var preset = presetOf(trial);
      if (!byPreset[preset]) {
        byPreset[preset] = { q1: [], q2: [], q3: [], q4: [], q5: [] };
        counts[preset] = { n: 0, yes: 0, maybe: 0, no: 0 };
      }
      QUESTIONS.forEach(function (q) {
        byPreset[preset][q].push(answers[q]);
      });
      counts[preset].n += 1;
      if (answers.q5 === 1) counts[preset].yes += 1;
      else if (answers.q5 === 2) counts[preset].maybe += 1;
      else if (answers.q5 === 3) counts[preset].no += 1;
    });

    var presets = Object.keys(byPreset).sort();
    var rows = presets.map(function (preset) {
      var values = byPreset[preset];
      var count = counts[preset];
      return (
        "<tr><td>" +
        escapeHtml(preset) +
        "</td><td>" +
        count.n +
        "</td><td>" +
        fmtMean(values.q1) +
        "</td><td>" +
        fmtMean(values.q2) +
        "</td><td>" +
        fmtMean(values.q3) +
        "</td><td>" +
        fmtMean(values.q4) +
        "</td><td>" +
        fmtMean(values.q5) +
        "</td><td>" +
        count.yes +
        " / " +
        count.maybe +
        " / " +
        count.no +
        "</td></tr>"
      );
    });
    els.summaryBody.innerHTML = rows.join("");
    els.csvFallback.value = buildCsv();

    window.scrollTo(0, 0);
    els.summaryTitle.focus();
  }

  function restart() {
    var ok = window.confirm("Start over? This erases the answers saved on this computer for this round.");
    if (!ok) return;
    stopAll();
    try {
      window.localStorage.removeItem(STORE_KEY);
    } catch (err) {
      /* Nothing to clear. */
    }
    STATE = freshState();
    saveState();
    els.summaryView.hidden = true;
    renderTrial();
  }

  // -------------------------------------------------------------------------
  // CSV
  // -------------------------------------------------------------------------

  function presetOf(trial) {
    try {
      return window.atob(trial.p);
    } catch (err) {
      return "unknown";
    }
  }

  function audioSource(path) {
    if (!AUDIO_BASE) return path;
    try {
      return new URL(path, AUDIO_BASE).href;
    } catch (err) {
      return path;
    }
  }

  function csvCell(value) {
    var text = value === null || value === undefined ? "" : String(value);
    if (/[",\r\n]/.test(text)) return '"' + text.replace(/"/g, '""') + '"';
    return text;
  }

  function buildCsv() {
    var rows = [
      ["trial", "voice", "phrase", "preset", "order", "q1", "q2", "q3", "q4", "q5", "notes", "utc"]
    ];
    DATA.trials.forEach(function (trial) {
      var answers = STATE.answers[trial.trial];
      if (!answers || !isComplete(answers)) return;
      rows.push([
        trial.trial,
        trial.voice,
        trial.phrase,
        presetOf(trial),
        trial.order,
        answers.q1,
        answers.q2,
        answers.q3,
        answers.q4,
        answers.q5,
        answers.notes || "",
        answers.completedUtc || ""
      ]);
    });
    return (
      rows
        .map(function (row) {
          return row.map(csvCell).join(",");
        })
        .join("\n") + "\n"
    );
  }

  function downloadCsv(statusElement) {
    var text = buildCsv();
    var rows = text.split("\n").length - 2;
    if (rows < 1) {
      statusElement.textContent = "Nothing to export yet — finish at least one trial.";
      return;
    }
    var blob = new Blob([text], { type: "text/csv;charset=utf-8" });
    var url = URL.createObjectURL(blob);
    var link = document.createElement("a");
    link.href = url;
    link.download = "rasp-hr2-" + DATA.seed + "-" + timestamp() + ".csv";
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(function () {
      URL.revokeObjectURL(url);
    }, 4000);
    statusElement.textContent = "CSV downloaded (" + rows + " trials).";
  }

  // -------------------------------------------------------------------------
  // Utilities
  // -------------------------------------------------------------------------

  function fmtMean(values) {
    if (!values || !values.length) return "—";
    var sum = values.reduce(function (total, value) {
      return total + value;
    }, 0);
    return (sum / values.length).toFixed(2);
  }

  function escapeHtml(text) {
    return String(text).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  function nowIso() {
    return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
  }

  function timestamp() {
    function pad(value) {
      return (value < 10 ? "0" : "") + value;
    }
    var d = new Date();
    return (
      d.getUTCFullYear() +
      pad(d.getUTCMonth() + 1) +
      pad(d.getUTCDate()) +
      "-" +
      pad(d.getUTCHours()) +
      pad(d.getUTCMinutes()) +
      pad(d.getUTCSeconds())
    );
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
