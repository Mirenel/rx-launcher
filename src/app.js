window.addEventListener("DOMContentLoaded", function () {
  var _invoke = window.__TAURI__.core.invoke;
  var appWindow = window.__TAURI__.window.getCurrentWindow();

  function invoke(cmd, args, ms) {
    var p = _invoke(cmd, args);
    if (!ms) return p;
    return Promise.race([
      p,
      new Promise(function (_, reject) {
        setTimeout(function () { reject('Command timed out'); }, ms);
      })
    ]);
  }

  // ── Toast notifications ─────────────────────────────────
  var toastContainer = document.getElementById('toast-container');
  function showToast(message) {
    var toast = document.createElement('div');
    toast.className = 'toast';
    toast.textContent = message;
    toastContainer.appendChild(toast);
    setTimeout(function () { toast.classList.add('toast--visible'); }, 10);
    setTimeout(function () {
      toast.classList.remove('toast--visible');
      setTimeout(function () { toast.remove(); }, 300);
    }, 2500);
  }

  // ── Plugin APIs ─────────────────────────────────────────
  var tauriFs = window.__TAURI__.fs;
  var tauriDialog = window.__TAURI__.dialog;
  var tauriProcess = window.__TAURI__.process;
  var tauriEvent = window.__TAURI__.event;
  var BaseDir = tauriFs.BaseDirectory;

  // ── Settings (persisted via fs plugin to AppConfig) ─────
  var SETTINGS_FILE = 'settings.json';
  var settings = { game_path: null, installed_patch_version: null, keep_open: false, minimize_to_tray: false, last_news_date: null };
  var config = null;

  function loadSettings() {
    return tauriFs.readTextFile(SETTINGS_FILE, { baseDir: BaseDir.AppConfig })
      .then(function (text) {
        try {
          var parsed = JSON.parse(text);
          if (parsed && typeof parsed === 'object') {
            // Only accept known keys with expected types
            if (typeof parsed.game_path === 'string') settings.game_path = parsed.game_path;
            if (typeof parsed.installed_patch_version === 'string') settings.installed_patch_version = parsed.installed_patch_version;
            if (typeof parsed.keep_open === 'boolean') settings.keep_open = parsed.keep_open;
            if (typeof parsed.minimize_to_tray === 'boolean') settings.minimize_to_tray = parsed.minimize_to_tray;
            if (typeof parsed.last_news_date === 'string') settings.last_news_date = parsed.last_news_date;
          }
        } catch (e) { /* corrupt file, use defaults */ }
      })
      .catch(function () { /* file doesn't exist yet */ });
  }

  function saveSettings() {
    var data = JSON.stringify(settings, null, 2);
    return tauriFs.writeTextFile(SETTINGS_FILE, data, { baseDir: BaseDir.AppConfig })
      .catch(function () {
        // AppConfig directory may not exist on first run — create and retry
        return tauriFs.mkdir('.', { baseDir: BaseDir.AppConfig, recursive: true })
          .then(function () {
            return tauriFs.writeTextFile(SETTINGS_FILE, data, { baseDir: BaseDir.AppConfig });
          });
      });
  }

  // ── Titlebar controls ───────────────────────────────────
  document.getElementById('btn-minimize').addEventListener('click', function () {
    appWindow.minimize();
  });

  document.getElementById('btn-close').addEventListener('click', function () {
    tauriProcess.exit(0);
  });

  document.getElementById('drag-zone').addEventListener('mousedown', function (e) {
    if (e.buttons === 1) {
      appWindow.startDragging();
    }
  });

  // ── Tab switching ───────────────────────────────────────
  var newsNavBtn = document.querySelector('.nav-item[data-tab="news"]');
  document.querySelectorAll('.nav-item[data-tab]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      document.querySelectorAll('.nav-item').forEach(function (b) { b.classList.remove('active'); });
      document.querySelectorAll('.tab-panel').forEach(function (p) { p.classList.remove('active'); });
      btn.classList.add('active');
      var panel = document.getElementById('tab-' + btn.dataset.tab);
      if (panel) panel.classList.add('active');
      if (btn.dataset.tab === 'news' && latestNewsDate) {
        newsNavBtn.classList.remove('nav-item--unread');
        settings.last_news_date = latestNewsDate;
        saveSettings();
      }
    });
  });
  var latestNewsDate = null;

  // ── Element references ──────────────────────────────────
  var statusDot = document.getElementById('status-dot');
  var statusLabel = document.getElementById('status-label');
  var statusPlayers = document.getElementById('status-players');
  var newsList = document.getElementById('news-list');
  var patchCurrentEl = document.getElementById('patch-current');
  var patchInstalledEl = document.getElementById('patch-installed');
  var sidebarPatchStatus = document.getElementById('sidebar-patch-status');
  var wowExeStatus = document.getElementById('wow-exe-status');
  var patchInstallPath = document.getElementById('patch-install-path');
  var downloadBtn = document.getElementById('download-patch-btn');
  var patchStatus = document.getElementById('patch-status');
  var patchLog = document.getElementById('patch-log');
  var patchNoPath = document.getElementById('patch-no-path');
  var patchProgress = document.getElementById('patch-progress');
  var patchProgressFill = document.getElementById('patch-progress-fill');
  var patchProgressText = document.getElementById('patch-progress-text');
  var gamePathInput = document.getElementById('game-path');
  var playBtn = document.getElementById('play-btn');
  var patchBtn = document.getElementById('patch-btn');
  var realmlistStatus = document.getElementById('realmlist-status');
  var patchModal = document.getElementById('patch-modal');
  var modalCancel = document.getElementById('modal-cancel');
  var modalConfirm = document.getElementById('modal-confirm');
  var modalWarnTitle = document.getElementById('modal-warn-title');
  var modalWarnBody = document.getElementById('modal-warn-body');
  var updateModal = document.getElementById('update-modal');
  var updateModalBody = document.getElementById('update-modal-body');
  var updateSkip = document.getElementById('update-skip');
  var updateDownload = document.getElementById('update-download');
  var updateFromPlay = false;
  var keepOpenCb = document.getElementById('keep-open-cb');
  var minimizeTrayCb = document.getElementById('minimize-tray-cb');
  var linkModal = document.getElementById('link-modal');
  var linkModalUrl = document.getElementById('link-modal-url');
  var linkCancel = document.getElementById('link-cancel');
  var linkOpen = document.getElementById('link-open');
  var changelogBtn = document.getElementById('changelog-btn');
  var changelogModal = document.getElementById('changelog-modal');
  var changelogTitle = document.getElementById('changelog-title');
  var changelogContent = document.getElementById('changelog-content');
  var changelogClose = document.getElementById('changelog-close');
  var repairBtn = document.getElementById('repair-btn');
  var repairModal = document.getElementById('repair-modal');
  var repairCancel = document.getElementById('repair-cancel');
  var repairConfirm = document.getElementById('repair-confirm');
  var uninstallBtn = document.getElementById('uninstall-btn');
  var uninstallModal = document.getElementById('uninstall-modal');
  var uninstallCancel = document.getElementById('uninstall-cancel');
  var uninstallConfirm = document.getElementById('uninstall-confirm');

  var launcherUpdateModal = document.getElementById('launcher-update-modal');
  var launcherUpdateVersion = document.getElementById('launcher-update-version');
  var launcherUpdateNotes = document.getElementById('launcher-update-notes');
  var launcherUpdateDl = document.getElementById('launcher-update-dl');
  var launcherUpdateStatus = document.getElementById('launcher-update-status');
  var launcherUpdateFill = document.getElementById('launcher-update-fill');
  var launcherUpdateActions = document.getElementById('launcher-update-actions');
  var launcherUpdateLater = document.getElementById('launcher-update-later');
  var launcherUpdateNow = document.getElementById('launcher-update-now');

  var pendingLinkUrl = null;
  var hasGamePath = false;
  var patchInstalled = false;
  var patchOutdated = false;

  // ── Server status ───────────────────────────────────────
  var statusChecking = false;
  function checkStatus() {
    if (statusChecking) return;
    statusChecking = true;
    invoke('check_server_status', {}, 8000).then(function (result) {
      if (result.online) {
        statusDot.className = 'status-dot online';
        statusLabel.textContent = 'Online';
        statusPlayers.textContent = result.players + ' player' + (result.players !== 1 ? 's' : '') + ' online';
      } else {
        statusDot.className = 'status-dot offline';
        statusLabel.textContent = 'Offline';
        statusPlayers.textContent = '';
      }
    }).catch(function () {
      statusDot.className = 'status-dot offline';
      statusLabel.textContent = 'Offline';
      statusPlayers.textContent = '';
    }).finally(function () {
      statusChecking = false;
    });
  }

  // ── News ────────────────────────────────────────────────
  function loadNews() {
    invoke('get_news', {}, 10000).then(function (items) {
      if (!items || items.length === 0) {
        newsList.innerHTML = '<div class="news-placeholder">No news yet.</div>';
        return;
      }
      newsList.innerHTML = '';

      items.forEach(function (item) {
        var entry = document.createElement('article');
        var hasImg = item.image && item.image.length > 0;
        var cls = 'news-entry';
        if (item.featured) cls += ' news-entry--featured';
        if (hasImg && !item.featured) cls += ' news-entry--has-img';
        entry.className = cls;

        var arrowHtml = item.url
          ? '<button class="news-entry__arrow" data-url="' + esc(item.url) + '">' +
              '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' +
                '<path d="M9 18l6-6-6-6"/>' +
              '</svg>' +
            '</button>'
          : '';

        var imgHtml = '';
        if (hasImg && item.featured) {
          imgHtml = '<div class="news-entry__hero" data-bg="' + esc(item.image) + '"></div>';
        } else if (hasImg) {
          imgHtml = '<div class="news-entry__thumb" data-bg="' + esc(item.image) + '"></div>';
        }

        var textHtml =
          '<div class="news-entry__text">' +
            '<div class="news-entry__head">' +
              '<div class="news-entry__meta">' +
                '<span class="news-entry__tag">' + esc(item.tag) + '</span>' +
                '<h3 class="news-entry__title">' + esc(item.title) + '</h3>' +
              '</div>' +
              '<time class="news-entry__date">' + esc(item.date) + '</time>' +
            '</div>' +
            '<div class="news-entry__rule"></div>' +
            '<p class="news-entry__body">' + esc(item.body) + '</p>' +
          '</div>';

        entry.innerHTML = imgHtml + textHtml + arrowHtml;
        newsList.appendChild(entry);
      });

      // Apply background images from data attributes (CSP-safe, origin-validated)
      newsList.querySelectorAll('[data-bg]').forEach(function (el) {
        var src = el.getAttribute('data-bg').replace(/[\\'"();\s]/g, '');
        if (src && src.indexOf('https://projectrx.net/') === 0) {
          el.style.backgroundImage = "url('" + src + "')";
        }
      });

      // Check for unread news
      latestNewsDate = items[0].date;
      if (settings.last_news_date !== latestNewsDate) {
        var newsActive = newsNavBtn.classList.contains('active');
        if (newsActive) {
          settings.last_news_date = latestNewsDate;
          saveSettings();
        } else {
          newsNavBtn.classList.add('nav-item--unread');
        }
      }

      // Wire up arrow link buttons
      newsList.querySelectorAll('.news-entry__arrow[data-url]').forEach(function (btn) {
        btn.addEventListener('click', function () {
          pendingLinkUrl = btn.getAttribute('data-url');
          linkModalUrl.textContent = pendingLinkUrl;
          showModal(linkModal);
        });
      });
    }).catch(function () {
      newsList.innerHTML = '<div class="news-placeholder">Could not load news.</div>';
    });
  }

  // ── Patch display ───────────────────────────────────────
  function updatePatchInstallPath() {
    if (settings.game_path) {
      patchInstallPath.textContent = 'Patch will be installed in ' + settings.game_path;
    } else {
      patchInstallPath.textContent = '';
    }
  }

  function checkWowExe() {
    if (!settings.game_path) {
      wowExeStatus.textContent = 'No game directory set';
      wowExeStatus.className = 'patch-card__value';
      return;
    }
    invoke('check_wow_exe', { gamePath: settings.game_path }).then(function (result) {
      if (result.status === 'patched') {
        wowExeStatus.textContent = 'Patched';
        wowExeStatus.className = 'patch-value patch-up-to-date';
      } else if (result.status === 'original') {
        wowExeStatus.textContent = 'Not patched';
        wowExeStatus.className = 'patch-value patch-not-installed';
      } else {
        wowExeStatus.textContent = 'Not found';
        wowExeStatus.className = 'patch-value patch-not-installed';
      }
    }).catch(function () {
      wowExeStatus.textContent = 'Error';
      wowExeStatus.className = 'patch-value patch-not-installed';
    });
  }

  function updatePatchDisplay() {
    if (!config) return;

    var currentVersion = config.patch_version;
    var installedVersion = settings.installed_patch_version;

    patchCurrentEl.textContent = currentVersion;

    if (installedVersion) {
      patchInstalledEl.textContent = installedVersion;
      patchInstalledEl.className = 'patch-card__value';

      if (installedVersion === currentVersion) {
        patchInstalled = true;
        patchOutdated = false;
        patchInstalledEl.classList.add('patch-up-to-date');
        sidebarPatchStatus.textContent = 'Up to date';
        sidebarPatchStatus.className = 'sidebar-patch-status up-to-date';
        downloadBtn.classList.add('hidden');
        patchStatus.classList.remove('hidden');
        if (hasGamePath) {
          uninstallBtn.classList.remove('hidden');
          repairBtn.classList.remove('hidden');
        }
      } else {
        patchInstalled = true;
        patchOutdated = true;
        patchInstalledEl.classList.add('patch-outdated');
        sidebarPatchStatus.textContent = 'Update available';
        sidebarPatchStatus.className = 'sidebar-patch-status outdated';
        if (hasGamePath) downloadBtn.disabled = false;
        downloadBtn.textContent = 'Update Patch';
        downloadBtn.classList.remove('hidden');
        patchStatus.classList.add('hidden');
        if (hasGamePath) {
          uninstallBtn.classList.remove('hidden');
          repairBtn.classList.remove('hidden');
        }
      }
    } else {
      patchInstalled = false;
      patchOutdated = false;
      patchInstalledEl.textContent = 'Not Installed';
      patchInstalledEl.className = 'patch-value patch-not-installed';
      sidebarPatchStatus.textContent = 'Patch required';
      sidebarPatchStatus.className = 'sidebar-patch-status';
      if (hasGamePath) downloadBtn.disabled = false;
      downloadBtn.textContent = 'Download Patch';
      downloadBtn.classList.remove('hidden');
      patchStatus.classList.add('hidden');
      uninstallBtn.classList.add('hidden');
      repairBtn.classList.add('hidden');
    }

    updatePatchInstallPath();
  }

  // ── Patch download ──────────────────────────────────────
  function patchLogLine(text, cls) {
    var line = document.createElement('div');
    line.className = 'log-line' + (cls ? ' ' + cls : '');
    line.textContent = '> ' + text;
    patchLog.appendChild(line);
    patchLog.scrollTop = patchLog.scrollHeight;
  }

  function setProgress(pct) {
    patchProgressFill.style.width = pct + '%';
    patchProgressText.textContent = pct + '%';
  }

  var fadeOutTimer = null;
  function fadeOutProgress(delayMs) {
    fadeOutTimer = setTimeout(function () {
      fadeOutTimer = null;
      patchProgress.classList.add('fade-out');
      var cleared = false;
      function clear() {
        if (cleared) return;
        cleared = true;
        patchProgress.classList.remove('visible', 'fade-out');
      }
      patchProgress.addEventListener('transitionend', function handler() {
        patchProgress.removeEventListener('transitionend', handler);
        clear();
      });
      setTimeout(clear, 800);
    }, delayMs);
  }

  var downloading = false;
  function startPatchDownload(force) {
    if (!hasGamePath) return;
    if (downloading) return;
    if (!force && downloadBtn.disabled) return;
    downloading = true;
    downloadBtn.disabled = true;
    repairBtn.disabled = true;
    if (fadeOutTimer) { clearTimeout(fadeOutTimer); fadeOutTimer = null; }
    patchProgress.classList.remove('fade-out');
    patchProgress.classList.add('visible');
    patchLog.innerHTML = '';
    patchLog.classList.add('visible');
    setProgress(0);

    var unlisten = null;
    var label = force ? 'Repair' : 'Patch';

    patchLogLine('Initializing ' + label.toLowerCase() + ' process...');
    setProgress(2);

    tauriEvent.listen('download-progress', function (event) {
      var d = event.payload;
      setProgress(10 + Math.round(d.percent * 0.85));
      if (d.log) {
        var cls = d.message.indexOf('installed') !== -1 ? 'log-ok' : '';
        patchLogLine(d.message, cls);
      } else {
        patchProgressText.textContent = d.message;
      }
    }).then(function (fn) {
      unlisten = fn;
      return invoke('check_wow_exe', { gamePath: settings.game_path });
    }).then(function (result) {
      setProgress(5);
      if (result.status === 'patched' && !force) {
        patchLogLine('Wow.exe already up to date.', 'log-ok');
      } else if (result.status === 'original') {
        patchLogLine('Backing up and patching Wow.exe...', 'log-warn');
        return invoke('patch_wow_exe', { gamePath: settings.game_path }).then(function (msg) {
          patchLogLine(msg, 'log-ok');
        });
      } else {
        patchLogLine(force ? 'Re-installing patched Wow.exe...' : 'Installing patched Wow.exe...');
        return invoke('patch_wow_exe', { gamePath: settings.game_path }).then(function (msg) {
          patchLogLine(msg, 'log-ok');
        });
      }
    }).then(function () {
      setProgress(10);
      patchLogLine(force ? 'Verifying and re-downloading corrupted files...' : 'Starting file downloads...');
      return invoke('download_patch', { gamePath: settings.game_path }, 600000);
    }).then(function (version) {
      setProgress(96);
      patchLogLine('All files downloaded and installed!', 'log-ok');
      settings.installed_patch_version = version;
      return saveSettings();
    }).then(function () {
      setProgress(100);
      patchLogLine(label + ' complete!', 'log-ok');
      repairBtn.disabled = false;
      downloading = false;
      updatePatchDisplay();
      checkWowExe();
      if (unlisten) unlisten();
      fadeOutProgress(2000);
    }).catch(function (e) {
      patchLogLine('Error: ' + (e || 'patch failed'), 'log-warn');
      downloadBtn.disabled = false;
      repairBtn.disabled = false;
      downloading = false;
      if (unlisten) unlisten();
      fadeOutProgress(3000);
    });
  }

  downloadBtn.addEventListener('click', function () {
    startPatchDownload(false);
  });

  repairBtn.addEventListener('click', function () {
    if (!hasGamePath) return;
    showModal(repairModal);
  });

  repairCancel.addEventListener('click', function () {
    hideModal(repairModal);
  });

  repairConfirm.addEventListener('click', function () {
    hideModal(repairModal);
    startPatchDownload(true);
  });

  // ── Game directory (dialog plugin) ──────────────────────
  document.getElementById('browse-btn').addEventListener('click', function () {
    tauriDialog.open({
      multiple: false,
      directory: true,
      title: 'Select WoW Directory'
    }).then(function (path) {
      if (path) {
        settings.game_path = path;
        saveSettings();
        showToast('Game directory set');
        gamePathInput.value = path;
        playBtn.disabled = false;
        patchBtn.disabled = false;
        hasGamePath = true;
        patchNoPath.classList.add('hidden');
        updatePatchInstallPath();
        checkWowExe();
        if (!patchInstalled || patchOutdated) downloadBtn.disabled = false;
        checkRealmlist();
      }
    }).catch(function () {});
  });

  // ── Realmlist ───────────────────────────────────────────
  function checkRealmlist() {
    if (!settings.game_path) return;
    invoke('check_realmlist', { gamePath: settings.game_path }).then(function (result) {
      realmlistStatus.textContent = result;
      realmlistStatus.className = result.indexOf('OK') === 0 ? 'setting-value ok' : 'setting-value err';
    }).catch(function (e) {
      realmlistStatus.textContent = String(e);
      realmlistStatus.className = 'setting-value err';
    });
  }

  patchBtn.addEventListener('click', function () {
    if (!settings.game_path) return;
    invoke('patch_realmlist', { gamePath: settings.game_path }).then(function (result) {
      realmlistStatus.textContent = result;
      realmlistStatus.className = 'setting-value ok';
      showToast('Realmlist updated');
    }).catch(function (e) {
      realmlistStatus.textContent = String(e);
      realmlistStatus.className = 'setting-value err';
    });
  });

  // ── After-launch checkboxes ─────────────────────────────
  keepOpenCb.addEventListener('change', function () {
    settings.keep_open = keepOpenCb.checked;
    minimizeTrayCb.disabled = !keepOpenCb.checked;
    if (!keepOpenCb.checked) {
      settings.minimize_to_tray = false;
      minimizeTrayCb.checked = false;
    }
    saveSettings();
    showToast('Settings saved');
  });

  minimizeTrayCb.addEventListener('change', function () {
    settings.minimize_to_tray = minimizeTrayCb.checked;
    saveSettings();
    showToast('Settings saved');
  });

  // ── Changelog modal ─────────────────────────────────────
  var changelogLoaded = false;

  changelogBtn.addEventListener('click', function () {
    showModal(changelogModal);
    if (changelogLoaded) return;
    invoke('get_changelog', {}, 10000).then(function (entries) {
      if (!entries || entries.length === 0) {
        changelogContent.innerHTML = '<div class="news-placeholder">No changelog available.</div>';
        return;
      }
      var html = '';
      entries.forEach(function (entry) {
        html += '<h3>' + esc(entry.version) + ' — ' + esc(entry.date) + '</h3><ul>';
        entry.changes.forEach(function (change) {
          html += '<li>' + esc(change) + '</li>';
        });
        html += '</ul>';
      });
      changelogContent.innerHTML = html;
      changelogLoaded = true;
    }).catch(function () {
      changelogContent.innerHTML = '<div class="news-placeholder">Could not load changelog.</div>';
    });
  });

  changelogClose.addEventListener('click', function () {
    hideModal(changelogModal);
  });

  // ── Warning modals ──────────────────────────────────────
  function showModal(overlay) { overlay.classList.add('visible'); }
  function hideModal(overlay) { overlay.classList.remove('visible'); }

  modalCancel.addEventListener('click', function () { hideModal(patchModal); });
  patchModal.addEventListener('click', function (e) {
    if (e.target === patchModal) hideModal(patchModal);
  });

  function launchGame() {
    invoke('launch_game', { gamePath: settings.game_path }).then(function () {
      if (!settings.keep_open) {
        tauriProcess.exit(0);
      } else if (settings.minimize_to_tray) {
        appWindow.hide();
      }
    }).catch(function () {
      modalWarnTitle.textContent = 'Launch Failed';
      modalWarnBody.textContent = 'Could not start the game. Make sure Wow.exe exists in your game directory.';
      showModal(patchModal);
    });
  }

  modalConfirm.addEventListener('click', function () {
    hideModal(patchModal);
    launchGame();
  });

  updateSkip.addEventListener('click', function () {
    hideModal(updateModal);
    if (updateFromPlay) launchGame();
  });

  updateModal.addEventListener('click', function (e) {
    if (e.target === updateModal) hideModal(updateModal);
  });

  updateDownload.addEventListener('click', function () {
    hideModal(updateModal);
    document.querySelectorAll('.nav-item').forEach(function (b) { b.classList.remove('active'); });
    document.querySelectorAll('.tab-panel').forEach(function (p) { p.classList.remove('active'); });
    var patchNavBtn = document.querySelector('.nav-item[data-tab="patch"]');
    if (patchNavBtn) patchNavBtn.classList.add('active');
    var patchPanel = document.getElementById('tab-patch');
    if (patchPanel) patchPanel.classList.add('active');
    downloadBtn.click();
  });

  // ── External link modal ────────────────────────────────
  linkCancel.addEventListener('click', function () { hideModal(linkModal); });
  linkModal.addEventListener('click', function (e) {
    if (e.target === linkModal) hideModal(linkModal);
  });
  linkOpen.addEventListener('click', function () {
    hideModal(linkModal);
    if (pendingLinkUrl) {
      invoke('open_url', { url: pendingLinkUrl });
      pendingLinkUrl = null;
    }
  });

  // ── Uninstall ──────────────────────────────────────────
  uninstallCancel.addEventListener('click', function () { hideModal(uninstallModal); });
  uninstallModal.addEventListener('click', function (e) {
    if (e.target === uninstallModal) hideModal(uninstallModal);
  });

  uninstallBtn.addEventListener('click', function () {
    if (!hasGamePath || downloading) return;
    showModal(uninstallModal);
  });

  uninstallConfirm.addEventListener('click', function () {
    hideModal(uninstallModal);
    uninstallBtn.classList.add('hidden');
    repairBtn.classList.add('hidden');
    patchLog.innerHTML = '';
    patchLog.classList.add('visible');
    patchLogLine('Uninstalling Project Rx content...');

    invoke('uninstall_patch', { gamePath: settings.game_path }).then(function (msg) {
      patchLogLine(msg, 'log-ok');
      settings.installed_patch_version = null;
      return saveSettings();
    }).then(function () {
      patchLogLine('Uninstall complete.', 'log-ok');
      updatePatchDisplay();
      checkWowExe();
    }).catch(function (e) {
      patchLogLine('Error: ' + (e || 'uninstall failed'), 'log-warn');
      uninstallBtn.classList.remove('hidden');
      repairBtn.classList.remove('hidden');
    });
  });

  // ── Play button ─────────────────────────────────────────
  playBtn.addEventListener('click', function () {
    if (!patchInstalled) {
      modalWarnTitle.textContent = 'Patch Not Installed';
      modalWarnBody.textContent = 'The Project Rx patch is not installed. You may experience issues without it.';
      showModal(patchModal);
      return;
    }
    if (patchOutdated) {
      updateFromPlay = true;
      updateModalBody.textContent = 'Your patch (' + esc(settings.installed_patch_version) + ') is outdated. Server requires ' + esc(config.patch_version) + '. Update now?';
      showModal(updateModal);
      return;
    }
    playBtn.disabled = true;
    invoke('verify_patch', { gamePath: settings.game_path }, 30000).then(function (bad) {
      playBtn.disabled = false;
      if (bad.length > 0) {
        modalWarnTitle.textContent = 'Integrity Check Failed';
        modalWarnBody.textContent = 'The following files failed verification: ' + bad.join(', ') + '. Use Repair on the Patch tab to fix this.';
        showModal(patchModal);
      } else {
        launchGame();
      }
    }).catch(function () {
      playBtn.disabled = false;
      launchGame();
    });
  });

  // ── Initialization ──────────────────────────────────────
  Promise.all([
    invoke('get_launcher_config'),
    loadSettings()
  ]).then(function (results) {
    config = results[0];

    if (settings.game_path) {
      gamePathInput.value = settings.game_path;
      playBtn.disabled = false;
      patchBtn.disabled = false;
      hasGamePath = true;
      patchNoPath.classList.add('hidden');
      checkRealmlist();
      checkWowExe();
    }

    keepOpenCb.checked = settings.keep_open;
    minimizeTrayCb.checked = settings.minimize_to_tray;
    minimizeTrayCb.disabled = !settings.keep_open;
    updatePatchDisplay();

    if (patchOutdated && hasGamePath) {
      updateFromPlay = false;
      updateModalBody.textContent = 'A new patch version is available (' + esc(config.patch_version) + '). You currently have ' + esc(settings.installed_patch_version) + ' installed. Would you like to download it?';
      showModal(updateModal);
    }

    checkStatus();
    var statusInterval = setInterval(checkStatus, 30000);
    document.addEventListener('visibilitychange', function () {
      if (document.hidden) {
        clearInterval(statusInterval);
        statusInterval = null;
      } else {
        checkStatus();
        statusInterval = setInterval(checkStatus, 30000);
      }
    });
    loadNews();
    checkLauncherUpdate();
  });

  // ── Launcher self-update ────────────────────────────────
  var pendingUpdate = null;

  launcherUpdateLater.addEventListener('click', function () { hideModal(launcherUpdateModal); });
  launcherUpdateModal.addEventListener('click', function (e) {
    if (e.target === launcherUpdateModal) hideModal(launcherUpdateModal);
  });

  launcherUpdateNow.addEventListener('click', function () {
    if (!pendingUpdate) return;
    var update = pendingUpdate;
    pendingUpdate = null;
    launcherUpdateActions.classList.add('hidden');
    launcherUpdateDl.classList.remove('hidden');
    launcherUpdateStatus.textContent = 'Downloading update...';

    var totalSize = 0;
    var downloadedSize = 0;

    update.downloadAndInstall(function (event) {
      if (event.event === 'Started') {
        totalSize = event.data.contentLength || 0;
      } else if (event.event === 'Progress') {
        downloadedSize += event.data.chunkLength || 0;
        if (totalSize > 0) {
          var pct = Math.min(99, Math.round(downloadedSize / totalSize * 100));
          launcherUpdateFill.style.width = pct + '%';
        }
      } else if (event.event === 'Finished') {
        launcherUpdateFill.style.width = '100%';
        launcherUpdateStatus.textContent = 'Installing update...';
      }
    }).then(function () {
      launcherUpdateStatus.textContent = 'Restarting...';
      tauriProcess.relaunch();
    }).catch(function () {
      launcherUpdateStatus.textContent = 'Update failed. Try again later.';
      launcherUpdateActions.classList.remove('hidden');
      launcherUpdateDl.classList.add('hidden');
    });
  });

  function checkLauncherUpdate() {
    var updater = window.__TAURI__ && window.__TAURI__.updater;
    if (!updater) return;
    updater.check().then(function (update) {
      if (!update) return;
      pendingUpdate = update;
      launcherUpdateVersion.textContent = update.version;
      launcherUpdateNotes.textContent = update.body || 'Bug fixes and improvements.';
      showModal(launcherUpdateModal);
    }).catch(function () { /* silent — update check is not critical */ });
  }

  // ── Utility ─────────────────────────────────────────────
  function esc(str) {
    if (!str) return '';
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
              .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }
});
