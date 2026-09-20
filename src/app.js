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

  var tauriFs = window.__TAURI__.fs;
  var tauriDialog = window.__TAURI__.dialog;
  var tauriProcess = window.__TAURI__.process;
  var tauriEvent = window.__TAURI__.event;
  var BaseDir = tauriFs.BaseDirectory;

  var SETTINGS_FILE = 'settings.json';
  var settings = { game_path: null, wine_prefix: null, installed_patch_version: null, installed_patch_path: null, keep_open: false, minimize_to_tray: false, last_news_date: null };
  var config = null;
  var settingsSaveQueue = Promise.resolve();

  function loadSettings() {
    return tauriFs.readTextFile(SETTINGS_FILE, { baseDir: BaseDir.AppConfig })
      .then(function (text) {
        try {
          var parsed = JSON.parse(text);
          if (parsed && typeof parsed === 'object') {
            // Ignore unknown or incorrectly typed persisted values.
            if (typeof parsed.game_path === 'string') settings.game_path = parsed.game_path;
            if (typeof parsed.wine_prefix === 'string' && parsed.wine_prefix.length > 0) settings.wine_prefix = parsed.wine_prefix;
            if (typeof parsed.installed_patch_version === 'string') settings.installed_patch_version = parsed.installed_patch_version;
            if (typeof parsed.installed_patch_path === 'string') settings.installed_patch_path = parsed.installed_patch_path;
            if (typeof parsed.keep_open === 'boolean') settings.keep_open = parsed.keep_open;
            if (typeof parsed.minimize_to_tray === 'boolean') settings.minimize_to_tray = parsed.minimize_to_tray;
            if (typeof parsed.last_news_date === 'string') settings.last_news_date = parsed.last_news_date;
          }
        } catch (e) {
          // Malformed settings must not prevent the launcher from starting.
        }
      })
      .catch(function () {
        // A missing settings file is expected on the first launch.
      });
  }

  function saveSettings() {
    var data = JSON.stringify(settings, null, 2);
    var write = function () {
      return tauriFs.writeTextFile(SETTINGS_FILE, data, { baseDir: BaseDir.AppConfig })
        .catch(function () {
          // The filesystem scope may not exist on first launch; create it and retry.
          return tauriFs.mkdir('.', { baseDir: BaseDir.AppConfig, recursive: true })
            .then(function () {
              return tauriFs.writeTextFile(SETTINGS_FILE, data, { baseDir: BaseDir.AppConfig });
            });
        });
    };
    // Serialize writes so a rapid checkbox/path/news interaction cannot
    // overwrite a newer settings snapshot with an older one.
    settingsSaveQueue = settingsSaveQueue.catch(function () {}).then(write);
    return settingsSaveQueue;
  }

  function saveSettingsWithFeedback(successMessage) {
    return saveSettings().then(function () {
      if (successMessage) showToast(successMessage);
    }).catch(function (e) {
      showToast('Could not save settings');
      throw e;
    });
  }

  document.addEventListener('contextmenu', function (e) { e.preventDefault(); });

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

  var newsNavBtn = document.querySelector('.nav-item[data-tab="news"]');
  var allNavItems = document.querySelectorAll('.nav-item');
  var allTabPanels = document.querySelectorAll('.tab-panel');
  var latestNewsDate = null;

  function switchToTab(name) {
    allNavItems.forEach(function (b) {
      var selected = b.dataset.tab === name;
      b.classList.toggle('active', selected);
      b.setAttribute('aria-selected', selected ? 'true' : 'false');
      // Keep every navigation tab in the Tab order; arrow keys provide an
      // alternate way to move between tabs.
      b.tabIndex = 0;
    });
    allTabPanels.forEach(function (p) {
      var selected = p.id === 'tab-' + name;
      p.classList.toggle('active', selected);
      p.hidden = !selected;
    });
    var btn = document.querySelector('.nav-item[data-tab="' + name + '"]');
    if (btn) btn.classList.add('active');
    var panel = document.getElementById('tab-' + name);
    if (panel) panel.classList.add('active');
    if (name === 'news' && latestNewsDate) {
      newsNavBtn.classList.remove('nav-item--unread');
      settings.last_news_date = latestNewsDate;
      saveSettings().catch(function () { showToast('Could not save news state'); });
    }
  }

  document.querySelectorAll('.nav-item[data-tab]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      switchToTab(btn.dataset.tab);
    });
    btn.addEventListener('keydown', function (e) {
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowRight' && e.key !== 'ArrowUp' && e.key !== 'ArrowLeft') return;
      e.preventDefault();
      var items = Array.prototype.slice.call(allNavItems);
      var direction = (e.key === 'ArrowDown' || e.key === 'ArrowRight') ? 1 : -1;
      var next = items[(items.indexOf(btn) + direction + items.length) % items.length];
      switchToTab(next.dataset.tab);
      next.focus();
    });
  });

  var statusDot = document.getElementById('status-dot');
  var statusLabel = document.getElementById('status-label');
  var statusPlayers = document.getElementById('status-players');
  var newsList = document.getElementById('news-list');
  var viewMoreNewsBtn = document.getElementById('view-more-news');
  var patchCurrentEl = document.getElementById('patch-current');
  var patchInstalledEl = document.getElementById('patch-installed');
  var sidebarPatchStatus = document.getElementById('sidebar-patch-status');
  var rxWowStatus = document.getElementById('rx-wow-status');
  var patchInstallPath = document.getElementById('patch-install-path');
  var downloadBtn = document.getElementById('download-patch-btn');
  var patchStatus = document.getElementById('patch-status');
  var patchLog = document.getElementById('patch-log');
  var patchNoPath = document.getElementById('patch-no-path');
  var patchProgress = document.getElementById('patch-progress');
  var patchProgressFill = document.getElementById('patch-progress-fill');
  var patchProgressText = document.getElementById('patch-progress-text');
  var gamePathInput = document.getElementById('game-path');
  var browseBtn = document.getElementById('browse-btn');
  var winePrefixSetting = document.getElementById('wine-prefix-setting');
  var winePrefixInput = document.getElementById('wine-prefix');
  var winePrefixStatus = document.getElementById('wine-prefix-status');
  var winePrefixClear = document.getElementById('wine-prefix-clear');
  var playBtn = document.getElementById('play-btn');
  var playBtnText = playBtn.querySelector('.play-btn-text');
  var patchBtn = document.getElementById('patch-btn');
  var realmlistStatus = document.getElementById('realmlist-status');
  var patchModal = document.getElementById('patch-modal');
  var modalCancel = document.getElementById('modal-cancel');
  var modalRepair = document.getElementById('modal-repair');
  var modalConfirm = document.getElementById('modal-confirm');
  var modalWarnTitle = document.getElementById('modal-warn-title');
  var modalWarnBody = document.getElementById('modal-warn-body');
  var modalWarnQuestion = document.getElementById('modal-warn-question');
  var updateModal = document.getElementById('update-modal');
  var updateModalBody = document.getElementById('update-modal-body');
  var updateSkip = document.getElementById('update-skip');
  var updateDownload = document.getElementById('update-download');
  var updateFromPlay = false;
  var updateGamePath = null;
  var launcherUpdateModal = document.getElementById('launcher-update-modal');
  var launcherUpdateModalBody = document.getElementById('launcher-update-modal-body');
  var launcherUpdateSkip = document.getElementById('launcher-update-skip');
  var launcherUpdateInstall = document.getElementById('launcher-update-install');
  var launcherUpdateBanner = document.getElementById('launcher-update-banner');
  var launcherUpdateBannerBody = document.getElementById('launcher-update-banner-body');
  var launcherUpdateBannerBtn = document.getElementById('launcher-update-banner-btn');
  var launcherUpdateNotice = null;
  var keepOpenCb = document.getElementById('keep-open-cb');
  var minimizeTrayCb = document.getElementById('minimize-tray-cb');
  var linkModal = document.getElementById('link-modal');
  var linkModalUrl = document.getElementById('link-modal-url');
  var linkCancel = document.getElementById('link-cancel');
  var linkOpen = document.getElementById('link-open');
  var discordBtn = document.getElementById('discord-btn');
  var discordModal = document.getElementById('discord-modal');
  var discordInviteUrlEl = document.getElementById('discord-invite-url');
  var discordCancel = document.getElementById('discord-cancel');
  var discordCopy = document.getElementById('discord-copy');
  var discordOpen = document.getElementById('discord-open');
  var changelogBtn = document.getElementById('changelog-btn');
  var launcherVersion = document.getElementById('launcher-version');
  var changelogModal = document.getElementById('changelog-modal');
  var changelogContent = document.getElementById('changelog-content');
  var changelogClose = document.getElementById('changelog-close');
  var repairBtn = document.getElementById('repair-btn');
  var repairModal = document.getElementById('repair-modal');
  var repairCancel = document.getElementById('repair-cancel');
  var repairConfirm = document.getElementById('repair-confirm');
  var uninstallBtn = document.getElementById('uninstall-btn');
  var uninstallModal = document.getElementById('uninstall-modal');
  var uninstallBody = document.getElementById('uninstall-body');
  var uninstallWarn = document.getElementById('uninstall-warn');
  var uninstallCancel = document.getElementById('uninstall-cancel');
  var uninstallConfirm = document.getElementById('uninstall-confirm');

  var gamePathStatus = document.getElementById('game-path-status');
  var setupBanner = document.getElementById('setup-banner');
  var pathWarnBanner = document.getElementById('path-warn-banner');
  var pathWarnText = document.getElementById('path-warn-text');
  var runtimeWarnBanner = document.getElementById('runtime-warn-banner');
  var runtimeWarnText = document.getElementById('runtime-warn-text');
  var runtimeWarnRetry = document.getElementById('runtime-warn-retry');

  var pendingLinkUrl = null;
  // Keep the community action available even when remote launcher metadata is unavailable.
  var discordInviteUrl = 'https://discord.gg/VTWnWbqJYE';
  var pendingLaunchAction = null;
  var hasGamePath = false;
  var runtimeReady = false;
  var runtimeChecking = false;
  var runtimeRecheckPending = false;
  var patchInstalled = false;
  var patchOutdated = false;
  var patchManifestReady = false;
  var gameOperationActive = false;

  function setGameOperationActive(active) {
    gameOperationActive = active;
    browseBtn.disabled = active;
    patchBtn.disabled = active || !hasGamePath;
    downloadBtn.disabled = active || !patchManifestReady || !hasGamePath || downloading;
    playBtn.disabled = active || !hasGamePath || !runtimeReady || downloading;
  }

  function sameGamePath(left, right) {
    if (typeof left !== 'string' || typeof right !== 'string') return false;
    var normalizedLeft = left.replace(/[\\/]+/g, '/').replace(/\/$/, '');
    var normalizedRight = right.replace(/[\\/]+/g, '/').replace(/\/$/, '');
    if (/^[A-Za-z]:\//.test(normalizedLeft) && /^[A-Za-z]:\//.test(normalizedRight)) {
      return normalizedLeft.toLowerCase() === normalizedRight.toLowerCase();
    }
    return normalizedLeft === normalizedRight;
  }

  function installedPatchVersionFor(path) {
    return settings.installed_patch_path && sameGamePath(settings.installed_patch_path, path)
      ? settings.installed_patch_version
      : null;
  }

  function setInstalledPatchVersion(path, version) {
    settings.installed_patch_path = path;
    settings.installed_patch_version = version;
  }

  function clearInstalledPatchVersion(path) {
    if (sameGamePath(settings.installed_patch_path, path)) {
      settings.installed_patch_path = null;
      settings.installed_patch_version = null;
    }
  }

  discordBtn.classList.remove('hidden');

  var statusChecking = false;
  function checkStatus() {
    if (statusChecking) return;
    statusChecking = true;
    invoke('check_server_status', {}, 8000).then(function (result) {
      if (result.online) {
        statusDot.className = 'status-dot online';
        statusLabel.textContent = 'Online';
        if (typeof result.players === 'number') {
          statusPlayers.textContent = result.players + ' player' + (result.players !== 1 ? 's' : '') + ' online';
        } else {
          statusPlayers.textContent = 'Player count unavailable';
        }
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

  function loadNews() {
    invoke('get_news', {}, 10000).then(function (items) {
      if (!items || items.length === 0) {
        newsList.innerHTML = '<div class="news-placeholder">No news yet.</div>';
        return;
      }
      newsList.innerHTML = '';
      items = items.map(function (item, index) {
        return { item: item, index: index, timestamp: Date.parse(item.date) };
      }).sort(function (a, b) {
        var aValid = !isNaN(a.timestamp);
        var bValid = !isNaN(b.timestamp);
        if (aValid && bValid && a.timestamp !== b.timestamp) return b.timestamp - a.timestamp;
        if (aValid !== bValid) return aValid ? -1 : 1;
        return a.index - b.index;
      }).map(function (wrapped) { return wrapped.item; });

      items.slice(0, 1).forEach(function (item) {
        var entry = document.createElement('article');
        var hasImg = item.image && item.image.length > 0;
        var cls = 'news-entry';
        if (item.featured) cls += ' news-entry--featured';
        if (hasImg && !item.featured) cls += ' news-entry--has-img';
        entry.className = cls;

        var allowedUrl = isAllowedProjectRxUrl(item.url);
        var arrowHtml = allowedUrl
          ? '<button class="news-entry__arrow" data-url="' + esc(item.url) + '" aria-label="Read ' + esc(item.title) + '">' +
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

      // Restrict remote backgrounds to the Project Rx HTTPS origin allowed by the CSP.
      newsList.querySelectorAll('[data-bg]').forEach(function (el) {
        var src = el.getAttribute('data-bg').replace(/[\\'"();\s]/g, '');
        if (src && src.indexOf('https://projectrx.net/') === 0) {
          el.style.backgroundImage = "url('" + src + "')";
        }
      });

      latestNewsDate = items[0].date;
      if (settings.last_news_date !== latestNewsDate) {
        var newsActive = newsNavBtn.classList.contains('active');
        if (newsActive) {
          settings.last_news_date = latestNewsDate;
          saveSettings().catch(function () { showToast('Could not save news state'); });
        } else {
          newsNavBtn.classList.add('nav-item--unread');
        }
      }

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

  viewMoreNewsBtn.addEventListener('click', function () {
    pendingLinkUrl = 'https://projectrx.net/#news';
    linkModalUrl.textContent = pendingLinkUrl;
    showModal(linkModal);
  });

  function isAllowedProjectRxUrl(value) {
    try {
      var parsed = new URL(value);
      return parsed.protocol === 'https:' &&
        parsed.username === '' && parsed.password === '' && parsed.port === '' &&
        (parsed.hostname === 'projectrx.net' || parsed.hostname === 'www.projectrx.net');
    } catch (e) {
      return false;
    }
  }

  function updatePatchInstallPath() {
    if (settings.game_path) {
      patchInstallPath.textContent = 'Patch will be installed in ' + settings.game_path;
    } else {
      patchInstallPath.textContent = '';
    }
  }

  function checkGameDirectory() {
    if (!settings.game_path) {
      gamePathStatus.textContent = 'No directory set';
      gamePathStatus.className = 'setting-value';
      rxWowStatus.textContent = 'Not installed';
      rxWowStatus.className = 'patch-card__value patch-not-installed';
      setGamePathValid(false);
      return Promise.resolve(false);
    }
    return invoke('check_game_directory', { gamePath: settings.game_path }).then(function (result) {
      var valid = !!result.has_wow && !!result.has_data && !!result.has_addons;
      rxWowStatus.textContent = result.has_rx_wow ? 'Installed' : 'Not installed';
      rxWowStatus.className = result.has_rx_wow
        ? 'patch-card__value patch-up-to-date'
        : 'patch-card__value patch-not-installed';
      setGamePathValid(valid);
      if (valid) {
        gamePathStatus.textContent = 'Valid game directory';
        gamePathStatus.className = 'setting-value ok';
      } else if (!result.has_wow) {
        gamePathStatus.textContent = 'Wow.exe missing';
        gamePathStatus.className = 'setting-value err';
      } else if (!result.has_data) {
        gamePathStatus.textContent = 'Data folder missing';
        gamePathStatus.className = 'setting-value err';
      } else {
        gamePathStatus.textContent = 'Interface/AddOns folder missing';
        gamePathStatus.className = 'setting-value err';
      }
      pathWarnBanner.classList.add('hidden');
      return valid;
    }).catch(function (e) {
      setGamePathValid(false);
      gamePathStatus.textContent = 'Directory not found';
      gamePathStatus.className = 'setting-value err';
      rxWowStatus.textContent = 'Unavailable';
      rxWowStatus.className = 'patch-card__value patch-not-installed';
      var msg = String(e);
      if (msg.indexOf('not found') !== -1) {
        pathWarnText.textContent = 'Game directory not found \u2014 update your path in Settings.';
      } else {
        pathWarnText.textContent = 'Game directory is invalid \u2014 update your path in Settings.';
      }
      pathWarnBanner.classList.remove('hidden');
      setupBanner.classList.add('hidden');
      return false;
    });
  }

  function setGamePathValid(valid) {
    hasGamePath = !!valid;
    updatePlayButtonState();
    patchBtn.disabled = !hasGamePath;
    patchNoPath.classList.toggle('hidden', hasGamePath);
    if (downloadBtn && (!patchInstalled || patchOutdated)) downloadBtn.disabled = !patchManifestReady || !hasGamePath || downloading;
    updatePatchDisplay();
  }

  function updatePlayButtonState() {
    playBtn.disabled = !hasGamePath || !runtimeReady || downloading || gameOperationActive;
  }

  function checkGameRuntime() {
    if (runtimeChecking) {
      runtimeRecheckPending = true;
      runtimeReady = false;
      updatePlayButtonState();
      return Promise.resolve(runtimeReady);
    }
    runtimeChecking = true;
    runtimeWarnRetry.disabled = true;
    runtimeWarnText.textContent = 'Checking the game runtime...';
    runtimeWarnBanner.classList.add('hidden');
    runtimeReady = false;
    updatePlayButtonState();

    return invoke('check_game_runtime', { winePrefix: settings.wine_prefix || null }, 10000).then(function (status) {
      updateWinePrefixVisibility(status);
      runtimeReady = !!status && status.ready === true;
      if (runtimeReady) {
        runtimeWarnBanner.classList.add('hidden');
      } else {
        runtimeWarnText.textContent = status && status.message
          ? status.message
          : 'The game runtime is unavailable. Install Wine with 32-bit support and try again.';
        runtimeWarnBanner.classList.remove('hidden');
      }
      updatePlayButtonState();
      return runtimeReady;
    }).catch(function () {
      runtimeReady = false;
      runtimeWarnText.textContent = 'Could not check the game runtime. Install Wine with 32-bit support and try again.';
      runtimeWarnBanner.classList.remove('hidden');
      updatePlayButtonState();
      return false;
    }).then(function (ready) {
      runtimeChecking = false;
      runtimeWarnRetry.disabled = false;
      if (runtimeRecheckPending) {
        runtimeRecheckPending = false;
        runtimeReady = false;
        updatePlayButtonState();
        checkGameRuntime();
      }
      return ready;
    });
  }

  runtimeWarnRetry.addEventListener('click', function () {
    checkGameRuntime();
  });

  function updateWinePrefixDisplay() {
    var selected = typeof settings.wine_prefix === 'string' && settings.wine_prefix.length > 0;
    winePrefixInput.value = selected ? settings.wine_prefix : '';
    winePrefixStatus.textContent = selected
      ? 'Custom prefix selected'
      : 'Inherited WINEPREFIX or ~/.wine';
    winePrefixClear.disabled = !selected;
  }

  function updateWinePrefixVisibility(status) {
    winePrefixSetting.classList.toggle('hidden', !status || status.runtime !== 'wine');
  }

  function updatePatchDisplay() {
    if (!config) return;

    var currentVersion = config.patch_version;
    var installedVersion = installedPatchVersionFor(settings.game_path);

    patchCurrentEl.textContent = patchManifestReady ? currentVersion : 'Unavailable';
    downloadBtn.disabled = !patchManifestReady || !hasGamePath || downloading;
    uninstallBtn.classList.add('hidden');
    repairBtn.classList.add('hidden');

    if (!patchManifestReady) {
      patchInstalled = !!installedVersion;
      patchOutdated = false;
      patchInstalledEl.textContent = installedVersion || 'Not Installed';
      patchInstalledEl.className = installedVersion
        ? 'patch-card__value patch-outdated'
        : 'patch-card__value patch-not-installed';
      sidebarPatchStatus.textContent = 'Content unavailable';
      sidebarPatchStatus.className = 'sidebar-patch-status outdated';
      patchStatus.classList.add('hidden');
      return;
    }

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
      patchInstalledEl.className = 'patch-card__value patch-not-installed';
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

  var speedHistory = [];

  function getSpeedText(dl, total) {
    if (total === 0) return '';
    var now = Date.now();
    if (speedHistory.length > 0 && dl < speedHistory[speedHistory.length - 1].bytes) {
      speedHistory = [];
    }
    speedHistory.push({ time: now, bytes: dl });
    while (speedHistory.length > 1 && now - speedHistory[0].time > 3000) {
      speedHistory.shift();
    }
    if (speedHistory.length < 2) return '';
    var first = speedHistory[0];
    var dt = (now - first.time) / 1000;
    if (dt < 0.5) return '';
    var speed = (dl - first.bytes) / dt;
    if (speed <= 0) return '';
    var parts = [];
    if (speed >= 1048576) parts.push((speed / 1048576).toFixed(1) + ' MB/s');
    else if (speed >= 1024) parts.push(Math.round(speed / 1024) + ' KB/s');
    else parts.push(Math.round(speed) + ' B/s');
    var remaining = total - dl;
    var eta = remaining / speed;
    if (eta > 1 && eta < 86400) {
      if (eta < 60) {
        parts.push('~' + Math.ceil(eta) + 's left');
      } else {
        var m = Math.floor(eta / 60);
        var s = Math.ceil(eta % 60);
        if (s === 60) { m++; s = 0; }
        parts.push('~' + m + ':' + (s < 10 ? '0' : '') + s + ' left');
      }
    }
    return parts.join(', ');
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
    var gamePath = settings.game_path;
    if (!gamePath) return;
    downloading = true;
    setGameOperationActive(true);
    downloadBtn.disabled = true;
    repairBtn.disabled = true;
    playBtn.disabled = true;
    speedHistory = [];
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
        var cls = d.message.indexOf('Backing up') !== -1 ? 'log-warn'
                : d.message.indexOf('installed') !== -1 ? 'log-ok' : '';
        patchLogLine(d.message, cls);
      } else {
        var st = d.bytes_total > 0 ? getSpeedText(d.bytes_downloaded, d.bytes_total) : '';
        patchProgressText.textContent = d.message + (st ? ' \u2014 ' + st : '');
      }
    }).then(function (fn) {
      unlisten = fn;
    }).then(function () {
      setProgress(10);
      patchLogLine(force ? 'Verifying and re-installing Project Rx content...' : 'Preparing Project Rx content...');
      // Patch mutations deliberately have no UI timeout: the backend may still be
      // writing files, and allowing a retry would start a concurrent mutation.
      return invoke('download_patch', { gamePath: gamePath, repair: !!force });
    }).then(function (version) {
      setProgress(96);
      patchLogLine('All content prepared and installed!', 'log-ok');
      setInstalledPatchVersion(gamePath, version);
      return saveSettings();
    }).then(function () {
      setProgress(100);
      patchLogLine(label + ' complete!', 'log-ok');
      repairBtn.disabled = false;
      downloading = false;
      setGameOperationActive(false);
      updatePlayButtonState();
      updatePatchDisplay();
      checkGameDirectory();
      // Do not overwrite a user's custom realmlist; only fill in a missing value.
      invoke('check_realmlist', { gamePath: gamePath }).catch(function (e) {
        if (String(e).indexOf('not found') !== -1) {
          return invoke('patch_realmlist', { gamePath: gamePath }).then(function () {
            patchLogLine('Realmlist set to projectrx.net', 'log-ok');
            checkRealmlist();
          });
        }
      }).catch(function () {});
      if (unlisten) unlisten();
      fadeOutProgress(2000);
    }).catch(function (e) {
      var msg = e ? String(e) : '';
      if (msg.indexOf('timed out') !== -1 || msg.indexOf('Timeout') !== -1) {
        msg = 'Download timed out. Check your internet connection and try again.';
      } else if (!msg) {
        msg = 'An unexpected error occurred. Try again, or use Repair if the problem persists.';
      }
      patchLogLine(msg, 'log-warn');
      downloadBtn.disabled = false;
      repairBtn.disabled = false;
      downloading = false;
      setGameOperationActive(false);
      updatePlayButtonState();
      updatePatchDisplay();
      checkGameDirectory();
      if (unlisten) unlisten();
      fadeOutProgress(5000);
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
  repairModal.addEventListener('click', function (e) {
    if (e.target === repairModal) hideModal(repairModal);
  });

  repairConfirm.addEventListener('click', function () {
    hideModal(repairModal);
    startPatchDownload(true);
  });

  browseBtn.addEventListener('click', function () {
    if (gameOperationActive) return;
    tauriDialog.open({
      multiple: false,
      directory: true,
      title: 'Select Game Directory'
    }).then(function (path) {
      if (path) {
        settings.game_path = path;
        gamePathInput.value = path;
        setupBanner.classList.add('hidden');
        pathWarnBanner.classList.add('hidden');
        setGamePathValid(false);
        saveSettingsWithFeedback('Game directory saved').then(function () {
          return checkGameDirectory();
        }).then(function (valid) {
          if (valid) checkRealmlist();
        }).catch(function () {});
      }
    }).catch(function () { showToast('Could not open folder picker'); });
  });

  document.getElementById('wine-prefix-browse').addEventListener('click', function () {
    tauriDialog.open({
      multiple: false,
      directory: true,
      title: 'Select Wine Prefix'
    }).then(function (path) {
      if (path) {
        settings.wine_prefix = path;
        updateWinePrefixDisplay();
        saveSettingsWithFeedback('Wine prefix saved').catch(function () {}).then(function () {
          return checkGameRuntime();
        });
      }
    }).catch(function () { showToast('Could not open folder picker'); });
  });

  winePrefixClear.addEventListener('click', function () {
    settings.wine_prefix = null;
    updateWinePrefixDisplay();
    saveSettingsWithFeedback('Using the default Wine prefix').catch(function () {}).then(function () {
      return checkGameRuntime();
    });
  });

  document.getElementById('setup-banner-btn').addEventListener('click', function () {
    document.getElementById('browse-btn').click();
  });

  document.getElementById('path-warn-btn').addEventListener('click', function () {
    document.getElementById('browse-btn').click();
  });

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
    if (!settings.game_path || patchBtn.disabled || gameOperationActive) return;
    var gamePath = settings.game_path;
    setGameOperationActive(true);
    patchBtn.disabled = true;
    invoke('patch_realmlist', { gamePath: gamePath }).then(function (result) {
      realmlistStatus.textContent = result;
      realmlistStatus.className = 'setting-value ok';
      showToast('Realmlist updated');
    }).catch(function (e) {
      realmlistStatus.textContent = String(e);
      realmlistStatus.className = 'setting-value err';
    }).finally(function () {
      patchBtn.disabled = false;
      setGameOperationActive(false);
    });
  });

  keepOpenCb.addEventListener('change', function () {
    settings.keep_open = keepOpenCb.checked;
    minimizeTrayCb.disabled = !keepOpenCb.checked;
    if (!keepOpenCb.checked) {
      settings.minimize_to_tray = false;
      minimizeTrayCb.checked = false;
    }
    saveSettingsWithFeedback('Settings saved').catch(function () {});
  });

  minimizeTrayCb.addEventListener('change', function () {
    settings.minimize_to_tray = minimizeTrayCb.checked;
    saveSettingsWithFeedback('Settings saved').catch(function () {});
  });

  var changelogLoaded = false;

  changelogBtn.addEventListener('click', function () {
    showModal(changelogModal);
    if (changelogLoaded) return;
    invoke('get_changelog', {}, 10000).then(function (entries) {
      changelogLoaded = true;
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
    }).catch(function () {
      changelogContent.innerHTML = '<div class="news-placeholder">Could not load changelog.</div>';
    });
  });

  changelogClose.addEventListener('click', function () {
    hideModal(changelogModal);
  });
  changelogModal.addEventListener('click', function (e) {
    if (e.target === changelogModal) hideModal(changelogModal);
  });

  var modalReturnFocus = new WeakMap();
  function showModal(overlay) {
    modalReturnFocus.set(overlay, document.activeElement);
    overlay.hidden = false;
    overlay.classList.add('visible');
    var target = overlay.querySelector('button:not([disabled]):not(.hidden)') || overlay.querySelector('[tabindex="-1"]');
    if (target) target.focus();
  }
  function hideModal(overlay) {
    overlay.classList.remove('visible');
    overlay.hidden = true;
    var target = modalReturnFocus.get(overlay);
    if (target && document.contains(target)) target.focus();
  }

  modalCancel.addEventListener('click', function () {
    hideModal(patchModal);
    modalRepair.classList.add('hidden');
    pendingLaunchAction = null;
    setGameOperationActive(false);
  });
  patchModal.addEventListener('click', function (e) {
    if (e.target === patchModal) {
      hideModal(patchModal);
      modalRepair.classList.add('hidden');
      pendingLaunchAction = null;
      setGameOperationActive(false);
    }
  });

  modalRepair.addEventListener('click', function () {
    hideModal(patchModal);
    modalRepair.classList.add('hidden');
    pendingLaunchAction = null;
    setGameOperationActive(false);
    startPatchDownload(true);
  });

  // Escape key closes the topmost visible modal
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Tab') {
      var visible = document.querySelector('.modal-overlay.visible');
      if (visible) {
        var focusable = visible.querySelectorAll('button:not([disabled]):not(.hidden), [href], input:not([disabled]), [tabindex]:not([tabindex="-1"])');
        if (focusable.length) {
          var first = focusable[0];
          var last = focusable[focusable.length - 1];
          if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
          else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
        }
      }
    }
    if (e.key !== 'Escape') return;
    var modals = [changelogModal, repairModal, uninstallModal, linkModal, discordModal, launcherUpdateModal, updateModal, patchModal];
    for (var i = 0; i < modals.length; i++) {
      if (modals[i].classList.contains('visible')) {
        hideModal(modals[i]);
        if (modals[i] === patchModal) {
          modalRepair.classList.add('hidden');
          pendingLaunchAction = null;
          setGameOperationActive(false);
        }
        if (modals[i] === updateModal) {
          updateFromPlay = false;
          updateGamePath = null;
          setGameOperationActive(false);
        }
        return;
      }
    }
  });

  function doLaunch(gamePath) {
    // Finish any queued checkbox/settings write before exiting or hiding the
    // launcher. Otherwise the process can terminate before the new choice is
    // persisted.
    saveSettings().catch(function () {}).then(function () {
      return invoke('launch_game', {
        gamePath: gamePath,
        winePrefix: settings.wine_prefix || null
      });
    }).then(function () {
      setGameOperationActive(false);
      if (!settings.keep_open) {
        tauriProcess.exit(0).catch(function () {
          showToast('Could not close the launcher');
        });
      } else if (settings.minimize_to_tray) {
        appWindow.hide().catch(function () {
          showToast('Could not hide the launcher to the system tray');
        });
      } else {
        showToast('Game launched');
      }
    }).catch(function (e) {
      setGameOperationActive(false);
      var message = String(e);
      modalWarnTitle.textContent = 'Launch Failed';
      modalWarnBody.textContent = message.indexOf('rx-wow.exe not found') !== -1
        ? 'rx-wow.exe is not installed in the selected game directory. Use Patch or Repair to install it first.'
        : message.indexOf('Wine prefix') !== -1
        ? message
        : message.indexOf('32-bit Wine') !== -1
        ? 'Wine is installed but 32-bit support is unavailable. Install Wine support for 32-bit Windows applications and try again.'
        : message.indexOf('Wine') !== -1
        ? 'Wine is required to launch the Windows game client on Linux. Install Wine and try again.'
        : 'Could not start rx-wow.exe. Make sure the game directory is accessible and try again.';
      modalWarnQuestion.classList.add('hidden');
      modalConfirm.classList.add('hidden');
      modalRepair.classList.remove('hidden');
      modalCancel.textContent = 'Close';
      pendingLaunchAction = null;
      showModal(patchModal);
    });
  }

  function launchGame(gamePath) {
    invoke('check_realmlist', { gamePath: gamePath }).then(function () {
      doLaunch(gamePath);
    }).catch(function (e) {
      var msg = String(e);
      var body;
      if (msg.indexOf('points elsewhere') !== -1) {
        body = 'Your realmlist is pointing to a different server. You can update it in Settings.';
      } else if (msg.indexOf('not found') !== -1) {
        body = 'The realmlist.wtf file was not found. You can set it in Settings.';
      } else {
        body = 'Could not verify your realmlist configuration. You can check it in Settings.';
      }
      modalWarnTitle.textContent = 'Realmlist Not Set';
      modalWarnBody.textContent = body;
      modalWarnQuestion.classList.remove('hidden');
      modalRepair.classList.add('hidden');
      modalConfirm.classList.remove('hidden');
      modalCancel.textContent = 'Cancel';
      pendingLaunchAction = function () { doLaunch(gamePath); };
      showModal(patchModal);
    });
  }

  modalConfirm.addEventListener('click', function () {
    hideModal(patchModal);
    modalRepair.classList.add('hidden');
    if (pendingLaunchAction) {
      var action = pendingLaunchAction;
      pendingLaunchAction = null;
      action();
    }
  });

  updateSkip.addEventListener('click', function () {
    var shouldLaunch = updateFromPlay;
    var gamePath = updateGamePath;
    updateFromPlay = false;
    updateGamePath = null;
    hideModal(updateModal);
    if (shouldLaunch && gamePath) launchGame(gamePath);
    else setGameOperationActive(false);
  });

  updateModal.addEventListener('click', function (e) {
    if (e.target === updateModal) {
      hideModal(updateModal);
      updateFromPlay = false;
      updateGamePath = null;
      setGameOperationActive(false);
    }
  });

  updateDownload.addEventListener('click', function () {
    updateFromPlay = false;
    updateGamePath = null;
    hideModal(updateModal);
    setGameOperationActive(false);
    switchToTab('patch');
    downloadBtn.click();
  });

  launcherUpdateSkip.addEventListener('click', function () {
    hideModal(launcherUpdateModal);
  });

  function showLauncherUpdatePrompt() {
    if (!launcherUpdateNotice) return;
    var message = 'Launcher version ' + launcherUpdateNotice.version + ' is available. Install it now?';
    if (typeof launcherUpdateNotice.notes === 'string' && launcherUpdateNotice.notes.trim()) {
      message += ' ' + launcherUpdateNotice.notes.trim();
    }
    launcherUpdateModalBody.textContent = message;
    showModal(launcherUpdateModal);
  }

  launcherUpdateBannerBtn.addEventListener('click', function () {
    showLauncherUpdatePrompt();
  });

  launcherUpdateModal.addEventListener('click', function (e) {
    if (e.target === launcherUpdateModal) hideModal(launcherUpdateModal);
  });

  launcherUpdateInstall.addEventListener('click', function () {
    launcherUpdateInstall.disabled = true;
    launcherUpdateSkip.disabled = true;
    launcherUpdateBannerBtn.disabled = true;
    launcherUpdateInstall.textContent = 'Updating...';
    launcherUpdateBannerBody.textContent = 'Downloading and verifying the launcher update...';
    launcherUpdateModalBody.textContent = 'Downloading and verifying the launcher update...';
    invoke('apply_launcher_update', null, 1200000).then(function () {
      // A normal successful apply exits the old process before this resolves.
      // This branch handles a race where the remote update disappears between
      // the check and the user's confirmation.
      launcherUpdateInstall.disabled = false;
      launcherUpdateSkip.disabled = false;
      launcherUpdateBannerBtn.disabled = false;
      launcherUpdateInstall.textContent = 'Install Update';
      launcherUpdateModalBody.textContent = 'No newer launcher update is available now.';
      launcherUpdateNotice = null;
      launcherUpdateBanner.hidden = true;
      launcherUpdateBanner.classList.add('hidden');
    }).catch(function (e) {
      launcherUpdateInstall.disabled = false;
      launcherUpdateSkip.disabled = false;
      launcherUpdateBannerBtn.disabled = false;
      launcherUpdateInstall.textContent = 'Install Update';
      launcherUpdateBannerBody.textContent = 'Launcher update available — select Install Update to try again.';
      launcherUpdateModalBody.textContent = 'The launcher update could not be installed: ' + String(e);
    });
  });

  linkCancel.addEventListener('click', function () { hideModal(linkModal); });
  linkModal.addEventListener('click', function (e) {
    if (e.target === linkModal) hideModal(linkModal);
  });
  linkOpen.addEventListener('click', function () {
    hideModal(linkModal);
    if (pendingLinkUrl) {
      invoke('open_url', { url: pendingLinkUrl }).catch(function () {
        showToast('Could not open link');
      });
      pendingLinkUrl = null;
    }
  });

  discordBtn.addEventListener('click', function () {
    if (!discordInviteUrl) return;
    discordInviteUrlEl.textContent = discordInviteUrl;
    showModal(discordModal);
  });
  discordCancel.addEventListener('click', function () { hideModal(discordModal); });
  discordModal.addEventListener('click', function (e) {
    if (e.target === discordModal) hideModal(discordModal);
  });
  discordCopy.addEventListener('click', function () {
    if (!discordInviteUrl) return;
    copyToClipboard(discordInviteUrl).then(function () {
      showToast('Discord invite copied');
    }).catch(function () {
      showToast('Could not copy Discord invite');
    });
  });
  discordOpen.addEventListener('click', function () {
    if (!discordInviteUrl) return;
    hideModal(discordModal);
    invoke('open_url', { url: discordInviteUrl }).catch(function () {
      showToast('Could not open Discord invite');
    });
  });

  function copyToClipboard(value) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(value);
    }
    var textarea = document.createElement('textarea');
    textarea.value = value;
    textarea.setAttribute('readonly', '');
    textarea.style.position = 'fixed';
    textarea.style.opacity = '0';
    document.body.appendChild(textarea);
    textarea.select();
    var copied = false;
    try { copied = document.execCommand('copy'); } catch (e) { copied = false; }
    textarea.remove();
    return copied ? Promise.resolve() : Promise.reject('Clipboard unavailable');
  }

  uninstallCancel.addEventListener('click', function () { hideModal(uninstallModal); });
  uninstallModal.addEventListener('click', function (e) {
    if (e.target === uninstallModal) hideModal(uninstallModal);
  });

  uninstallBtn.addEventListener('click', function () {
    if (!hasGamePath || downloading || gameOperationActive) return;
    uninstallBody.textContent = 'This will remove all Project Rx patches and addons. Your other game files will not be affected.';
    uninstallWarn.classList.add('hidden');
    showModal(uninstallModal);
  });

  uninstallConfirm.addEventListener('click', function () {
    var gamePath = settings.game_path;
    if (!gamePath || gameOperationActive) return;
    setGameOperationActive(true);
    hideModal(uninstallModal);
    uninstallBtn.classList.add('hidden');
    repairBtn.classList.add('hidden');
    patchLog.innerHTML = '';
    patchLog.classList.add('visible');
    patchLogLine('Uninstalling Project Rx content...');

    invoke('uninstall_patch', { gamePath: gamePath }).then(function (msg) {
      patchLogLine(msg, 'log-ok');
      clearInstalledPatchVersion(gamePath);
      return saveSettings();
    }).then(function () {
      patchLogLine('Uninstall complete.', 'log-ok');
      setGameOperationActive(false);
      updatePatchDisplay();
      checkGameDirectory();
    }).catch(function (e) {
      patchLogLine('Error: ' + (e || 'uninstall failed'), 'log-warn');
      setGameOperationActive(false);
      uninstallBtn.classList.remove('hidden');
      repairBtn.classList.remove('hidden');
    });
  });

  function continuePlayFlow() {
    if (!runtimeReady) {
      runtimeWarnBanner.classList.remove('hidden');
      return;
    }
    var gamePath = settings.game_path;
    if (!gamePath || gameOperationActive) return;
    if (!patchInstalled) {
      setGameOperationActive(true);
      modalWarnTitle.textContent = 'Patch Not Installed';
      modalWarnBody.textContent = 'The Project Rx patch is not installed. You may experience issues without it.';
      modalWarnQuestion.classList.remove('hidden');
      modalRepair.classList.remove('hidden');
      modalConfirm.classList.remove('hidden');
      modalCancel.textContent = 'Cancel';
      pendingLaunchAction = function () { launchGame(gamePath); };
      showModal(patchModal);
      return;
    }
    if (patchOutdated) {
      updateFromPlay = true;
      updateGamePath = gamePath;
      setGameOperationActive(true);
      updateModalBody.textContent = 'Your patch (' + installedPatchVersionFor(gamePath) + ') is outdated. Server requires ' + config.patch_version + '. Update now?';
      showModal(updateModal);
      return;
    }
    setGameOperationActive(true);
    playBtn.disabled = true;
    playBtnText.textContent = 'Verifying...';
    invoke('verify_patch', { gamePath: gamePath }, 30000).then(function (bad) {
      updatePlayButtonState();
      playBtnText.textContent = 'Play';
      if (bad.length > 0) {
        modalWarnTitle.textContent = 'Integrity Check Failed';
        modalWarnBody.textContent = 'The following files failed verification: ' + bad.join(', ') + '. Make sure your game directory is correct, then use Repair on the Patch tab.';
        modalWarnQuestion.classList.remove('hidden');
        modalRepair.classList.remove('hidden');
        modalConfirm.classList.remove('hidden');
        modalCancel.textContent = 'Cancel';
        pendingLaunchAction = function () { launchGame(gamePath); };
        showModal(patchModal);
      } else {
        launchGame(gamePath);
      }
    }).catch(function (e) {
      setGameOperationActive(false);
      updatePlayButtonState();
      playBtnText.textContent = 'Play';
      modalWarnTitle.textContent = 'Verification Unavailable';
      modalWarnBody.textContent = 'The launcher could not verify the patch: ' + String(e || 'unknown error') + '. You can use Repair, or explicitly continue at your own risk.';
      modalWarnQuestion.classList.remove('hidden');
      modalRepair.classList.remove('hidden');
      modalConfirm.classList.remove('hidden');
      modalCancel.textContent = 'Cancel';
      pendingLaunchAction = function () { launchGame(gamePath); };
      showModal(patchModal);
    });
  }

  playBtn.addEventListener('click', function () {
    continuePlayFlow();
  });

  Promise.all([
    invoke('get_launcher_config', null, 10000),
    loadSettings()
  ]).then(function (results) {
    config = results[0];
    changelogBtn.textContent = 'Changelog v' + config.launcher_version;
    launcherVersion.textContent = 'Launcher v' + config.launcher_version;

    if (settings.game_path) {
      gamePathInput.value = settings.game_path;
      setupBanner.classList.add('hidden');
      setGamePathValid(false);
      checkGameDirectory().then(function (valid) {
        if (valid) {
          checkRealmlist();
          if (patchOutdated) {
            updateFromPlay = false;
            updateGamePath = settings.game_path;
            setGameOperationActive(true);
            updateModalBody.textContent = 'A new patch version is available (' + config.patch_version + '). You currently have ' + (installedPatchVersionFor(settings.game_path) || 'an older version') + ' installed. Would you like to download it?';
            showModal(updateModal);
          }
        }
      });
    }

    keepOpenCb.checked = settings.keep_open;
    if (!settings.keep_open) settings.minimize_to_tray = false;
    minimizeTrayCb.checked = settings.minimize_to_tray;
    minimizeTrayCb.disabled = !settings.keep_open;
    updateWinePrefixDisplay();
    updatePatchDisplay();
    checkGameRuntime();

    // Content metadata is independently authenticated and may be hosted by
    // either configured Project Rx source. It must not prevent the launcher
    // shell from starting when both sources are temporarily unavailable.
    invoke('get_patch_manifest', null, 25000).then(function (info) {
      config.patch_version = info.version;
      patchManifestReady = true;
      updatePatchDisplay();
      if (settings.game_path && hasGamePath && patchOutdated) {
        updateFromPlay = false;
        updateGamePath = settings.game_path;
        setGameOperationActive(true);
        updateModalBody.textContent = 'A new patch version is available (' + config.patch_version + '). You currently have ' + (installedPatchVersionFor(settings.game_path) || 'an older version') + ' installed. Would you like to download it?';
        showModal(updateModal);
      }
    }).catch(function () {
      patchManifestReady = false;
      updatePatchDisplay();
    });

    checkStatus();
    var statusInterval = setInterval(checkStatus, 30000);
    document.addEventListener('visibilitychange', function () {
      if (document.hidden) {
        clearInterval(statusInterval);
        statusInterval = null;
      } else {
        checkStatus();
        if (!statusInterval) statusInterval = setInterval(checkStatus, 30000);
      }
    });
    loadNews();

    // Update discovery is asynchronous and never blocks launcher startup. The
    // user must confirm before any download or replacement begins.
    setTimeout(function () {
      invoke('check_launcher_update', null, 40000).then(function (notice) {
        if (!notice || !notice.version) return;
        launcherUpdateNotice = notice;
        launcherUpdateBannerBody.textContent = 'Launcher v' + notice.version + ' is available.';
        launcherUpdateBanner.hidden = false;
        launcherUpdateBanner.classList.remove('hidden');
        showLauncherUpdatePrompt();
      }).catch(function () {});
    }, 500);

    var tauriPath = window.__TAURI__.path;
    if (tauriPath && tauriPath.appConfigDir) {
      tauriPath.appConfigDir().then(function (dir) {
        document.getElementById('settings-data-path').textContent = 'Launcher data: ' + dir;
      }).catch(function () {});
    }
  }).catch(function () {
    statusLabel.textContent = 'Launcher unavailable';
    statusPlayers.textContent = '';
    showToast('Could not load launcher configuration');
  });


  function esc(str) {
    if (!str) return '';
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
              .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }
});
