(() => {
  const errors = [];
  let labelMetrics = null;
  const check = (name, ok) => { if (!ok) errors.push(name); };
  try {
    check('hotkey-space', typeof window.__crosshairHotkeyKeyName === 'function' && window.__crosshairHotkeyKeyName(' ') === 'Space');
    check('hotkey-letter', window.__crosshairHotkeyKeyName && window.__crosshairHotkeyKeyName('f') === 'F');
    check('viewport', innerWidth === 1200 && innerHeight === 780);
    check('ui-ready', window.__crosshairReady === true);
    check('protocol-origin', location.protocol === 'https:' && location.host === 'app.localhost');
    check('ipc-bridge', typeof window.ipc?.postMessage === 'function');
    check('true-black', getComputedStyle(document.body).backgroundColor === 'rgb(0, 0, 0)');
    check('header-60', Math.round(document.querySelector('.topbar')?.getBoundingClientRect().height || 0) === 60);
    const topbar = document.querySelector('.topbar');
    const tabsBox = document.querySelector('.tabs')?.getBoundingClientRect();
    check('header-layout', !!tabsBox && Math.abs((tabsBox.left + tabsBox.right) / 2 - innerWidth / 2) <= 1);
    check('header-brand-removed', !document.querySelector('.brand img') && !document.querySelector('.brand-icon'));
    check('header-controls-moved', !!topbar && !topbar.querySelector('.top-actions') && !topbar.querySelector('[data-action="settings"]'));
    const aimTab = document.querySelector('.tabs [role=tab][data-tab="aim"]');
    aimTab?.focus();
    aimTab?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
    check('tab-arrow-keys', document.body.dataset.tab === 'display' && document.activeElement?.dataset?.tab === 'display' && document.querySelector('.tabs [role=tab][data-tab="display"]')?.getAttribute('aria-selected') === 'true');
    document.querySelector('[data-tab="aim"]')?.click();
    const footer = document.querySelector('.statusbar');
    const badge = document.querySelector('#slotBadge');
    const footerBox = footer?.getBoundingClientRect();
    const badgeBox = badge?.getBoundingClientRect();
    check('footer-bottom', !!footerBox && Math.round(footerBox.height) === 46 && Math.abs(footerBox.bottom - innerHeight) <= 1);
    check('footer-module-controls', !!footer && footer.querySelectorAll('.quick-toggle').length === 3 && !!footer.querySelector('.statusbar-settings'));
    check('slot-chip-footer', !!badge && !!footer && footer.contains(badge) && !!badgeBox && badgeBox.top >= footerBox.top - 1 && badgeBox.bottom <= footerBox.bottom + 1);
    check('no-top-slot-chip', !!badge && !topbar.contains(badge));
    const topbarWidth = topbar.style.width;
    topbar.style.width = '1080px';
    const narrowTabs = document.querySelector('.tabs').getBoundingClientRect();
    check('header-layout-narrow', Math.abs((narrowTabs.left + narrowTabs.right) / 2 - 540) <= 1);
    topbar.style.width = topbarWidth;
    check('left-column-420', getComputedStyle(document.querySelector('.three-column')).gridTemplateColumns.startsWith('420px'));
    check('flat-cards', getComputedStyle(document.querySelector('.card')).boxShadow === 'none');
    const nonAppImages = Array.from(document.querySelectorAll('img[src^="data:image"]')).filter((img) => !img.closest('.app-icon'));
    check('no-base64-inline-images', nonAppImages.length === 0);
    check('four-tabs', document.querySelectorAll('.tab').length === 4);
    check('active-monitor', (document.querySelector('#aimMonitor')?.textContent || '').includes('AOC'));
    check('behavior-tooltips', document.querySelector('[data-bind="crosshair.hide_in_menus"]')?.getAttribute('aria-label')?.includes('Start, taskbar and app menus') && document.querySelectorAll('#aimVisibility .seg').length === 3 && document.querySelector('[data-bind="crosshair.hide_in_menus"]')?.closest('.row-toggle')?.querySelector('span')?.title.includes('Start, taskbar'));
    check('aim-visibility-control', document.querySelector('#aimVisibility [data-value="always"]') && document.querySelector('#aimVisibility [data-value="hide"]') && document.querySelector('#aimVisibility [data-value="only"]'));
    check('aim-copy-short', JSON.stringify(Array.from(document.querySelectorAll('#aimVisibility .seg')).map((el) => el.textContent.trim())) === JSON.stringify(['Always', 'Hide', 'Only']) && document.querySelector('.aim-visibility-label')?.textContent === 'When aiming');
    const visTrack = document.querySelector('#aimVisibility');
    const visLabel = document.querySelector('.aim-visibility .aim-visibility-label');
    const visCard = visTrack?.closest('.card');
    const visCardStyle = visCard ? getComputedStyle(visCard) : null;
    const visCardInner = visCard && visCardStyle
      ? visCard.getBoundingClientRect().width
        - parseFloat(visCardStyle.paddingLeft) - parseFloat(visCardStyle.paddingRight)
        - parseFloat(visCardStyle.borderLeftWidth) - parseFloat(visCardStyle.borderRightWidth)
      : 0;
    check('aim-visibility-stacked', !!visTrack && !!visLabel && visLabel.getBoundingClientRect().bottom <= visTrack.getBoundingClientRect().top + 1);
    check('aim-visibility-track', !!visTrack
      && getComputedStyle(visTrack).gridTemplateColumns.split(' ').length === 3
      && getComputedStyle(visTrack).borderRadius === '10px'
      && getComputedStyle(visTrack).padding === '3px'
      && getComputedStyle(visTrack).gap === '3px'
      && getComputedStyle(visTrack).backgroundColor === 'rgb(11, 13, 18)'
      && getComputedStyle(visTrack).borderTopColor === 'rgb(34, 37, 45)');
    check('aim-visibility-full-width', !!visTrack && Math.abs(visTrack.getBoundingClientRect().width - visCardInner) <= 1);
    const visActive = visTrack?.querySelector('.seg.active');
    const visIdle = visTrack?.querySelector('.seg:not(.active)');
    check('aim-visibility-active', !!visActive
      && getComputedStyle(visActive).backgroundColor === 'rgb(78, 168, 255)'
      && getComputedStyle(visActive).color === 'rgb(4, 16, 27)'
      && Number(getComputedStyle(visActive).fontWeight) >= 700
      && getComputedStyle(visActive).borderRadius === '7px');
    check('aim-visibility-inactive', !!visIdle
      && getComputedStyle(visIdle).backgroundColor === 'rgba(0, 0, 0, 0)'
      && getComputedStyle(visIdle).color === 'rgb(120, 128, 144)');
    check('aim-visibility-readable', !!visActive && !!visIdle
      && [visActive, visIdle].every((el) => parseFloat(getComputedStyle(el).fontSize) >= 13 && parseFloat(getComputedStyle(el).fontSize) <= 14));
    check('aim-visibility-clipping', !!visTrack
      && visTrack.scrollWidth <= visTrack.clientWidth + 1
      && Array.from(visTrack.querySelectorAll('.seg')).every((el) => el.scrollWidth <= el.clientWidth + 1));
    const aimRow = document.querySelector('#aimButtonRow');
    const aimButton = document.querySelector('#aimButton');
    const aimMenu = document.querySelector('#aimButtonMenu');
    check('aim-button-hidden-always', !!aimRow && aimRow.classList.contains('hidden') && (aimRow.getBoundingClientRect().height || 0) === 0);
    document.querySelector('[data-action="aim-visibility"][data-value="hide"]')?.click();
    check('aim-button-row-hide', !!aimRow && !aimRow.classList.contains('hidden') && aimRow.getBoundingClientRect().height > 0 && !!aimButton);
    const aimStyle = aimButton ? getComputedStyle(aimButton) : null;
    check('aim-button-custom-style', !!aimButton && aimButton.tagName === 'BUTTON' && aimButton.classList.contains('custom-trigger') && !!aimStyle
      && aimStyle.borderTopStyle === 'solid' && aimStyle.borderTopWidth === '1px'
      && aimStyle.borderTopColor === 'rgb(36, 36, 42)' && aimStyle.borderRadius === '8px');
    check('aim-button-default', !!aimMenu && aimMenu.classList.contains('custom-menu') && aimMenu.classList.contains('hidden')
      && aimButton?.getAttribute('aria-expanded') === 'false'
      && document.querySelector('#aimButtonLabel')?.textContent === 'Right Mouse'
      && aimMenu.querySelectorAll('.custom-option').length === 5
      && Array.from(aimMenu.querySelectorAll('.custom-option')).map((option) => option.dataset.label).join('|') === 'Right Mouse|Left Mouse|Middle Mouse|Mouse 4|Mouse 5'
      && aimMenu.querySelector('[data-value="right"]')?.classList.contains('active')
      && aimMenu.querySelector('[data-value="right"]')?.getAttribute('aria-selected') === 'true');
    aimButton?.click();
    check('aim-button-menu-open', !!aimButton && aimButton.getAttribute('aria-expanded') === 'true' && !!aimMenu && !aimMenu.classList.contains('hidden'));
    aimMenu?.querySelector('[data-value="mouse4"]')?.click();
    check('aim-button-command', window.__lastSent?.type === 'UpdateCrosshair' && window.__lastSent?.payload?.crosshair?.aimButton === 'mouse4'
      && window.__crosshairState.config.crosshair.activePreset === null
      && document.querySelector('#aimButtonLabel')?.textContent === 'Mouse 4'
      && aimButton?.getAttribute('aria-expanded') === 'false' && !!aimMenu && aimMenu.classList.contains('hidden'));
    aimButton?.focus();
    aimButton?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true, cancelable: true }));
    check('aim-button-keyboard-open', !!aimMenu && !aimMenu.classList.contains('hidden') && document.activeElement === aimMenu.querySelector('[data-value="right"]'));
    document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true, cancelable: true }));
    check('aim-button-keyboard-nav', document.activeElement === aimMenu?.querySelector('[data-value="left"]'));
    document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
    check('aim-button-keyboard-command', window.__lastSent?.payload?.crosshair?.aimButton === 'left'
      && window.__crosshairState.config.crosshair.activePreset === null
      && document.querySelector('#aimButtonLabel')?.textContent === 'Left Mouse'
      && !!aimMenu && aimMenu.classList.contains('hidden'));
    aimButton?.focus();
    aimButton?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true, cancelable: true }));
    document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    check('aim-button-keyboard-escape', !!aimMenu && aimMenu.classList.contains('hidden') && aimButton?.getAttribute('aria-expanded') === 'false' && document.activeElement === aimButton);
    aimButton?.click();
    aimMenu?.querySelector('[data-value="right"]')?.click();
    check('aim-button-restored-value', window.__lastSent?.payload?.crosshair?.aimButton === 'right' && document.querySelector('#aimButtonLabel')?.textContent === 'Right Mouse');
    document.querySelector('[data-action="aim-visibility"][data-value="only"]')?.click();
    check('aim-button-row-only', !!aimRow && !aimRow.classList.contains('hidden') && aimRow.getBoundingClientRect().height > 0);
    document.querySelector('[data-action="aim-visibility"][data-value="always"]')?.click();
    check('aim-button-restored', !!aimRow && aimRow.classList.contains('hidden'));
    check('image-format-copy', (document.querySelector('#dropzone')?.textContent || '').includes('BMP') && (document.querySelector('#dropzone')?.textContent || '').includes('ICO') && (document.querySelector('#dropzone')?.textContent || '').includes('Drop an image or GIF'));
    document.querySelector('[data-tab="aim"]')?.click();
    check('preview-image', (document.querySelector('#aimPreview')?.naturalWidth || 0) > 0);
    check('preview-route', (document.querySelector('#aimPreview')?.getAttribute('src') || '').startsWith('/preview'));
    check('preview-image-size', document.querySelector('#aimPreview')?.getBoundingClientRect().width > 300);
    const aimStage = document.querySelector('#aimStage')?.getBoundingClientRect();
    check('aim-preview-16-9', !!aimStage && Math.abs(aimStage.width / aimStage.height - 16 / 9) < 0.02);
    check('preset-picker', !!document.querySelector('#aimPresetTrigger') && !!document.querySelector('#aimPresetMenu'));
    check('preset-icon', (document.querySelector('#aimPresetIcon')?.naturalWidth || 0) > 0);
    check('preset-names', ['Pro Plus', 'Precision Dot', 'Tracking Ring', 'Ring + Dot', 'Triangle', 'Chevron', 'Open T-Cross'].every((name) => Array.from(document.querySelectorAll('#aimPresetMenu .preset-option')).some((node) => (node.querySelector('span')?.textContent || '').trim() === name)) && !/\b(Casual|Hairline)\b/.test(document.querySelector('#aimPresetMenu')?.innerText || ''));
    check('shape-controls', !!document.querySelector('#dotSizeRow') && !!document.querySelector('#outlineWidthRow'));
    document.querySelector('#fxModal')?.classList.add('hidden');
    document.querySelector('#backupModal')?.classList.add('hidden');
    document.querySelector('#colorTrigger')?.click();
    check('color-popover-open', !document.querySelector('#colorPopover')?.classList.contains('hidden') && !!document.querySelector('#colorSv') && !!document.querySelector('#colorHue') && !!document.querySelector('#colorHex'));
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    check('color-popover-escape', document.querySelector('#colorPopover')?.classList.contains('hidden'));
    check('active-tab-rounded', (() => { const tab = document.querySelector('.tab.active'); if (!tab) return false; const style = getComputedStyle(tab); return parseFloat(style.borderRadius) >= 8 && style.borderBottomWidth === style.borderTopWidth; })());
    const activeSwatch = document.querySelector('.swatch.active');
    const customColorActive = !!document.querySelector('#colorTrigger')?.classList.contains('active');
    check('swatch-selected-border', (!!activeSwatch && parseFloat(getComputedStyle(activeSwatch).borderTopWidth) >= 3) || customColorActive);
    check('custom-color-rainbow', getComputedStyle(document.querySelector('#colorTrigger')).backgroundImage.includes('conic-gradient'));
    check('collapsible-advanced', document.querySelectorAll('details.more-options').length === 2 && document.querySelector('#displayMore') && document.querySelector('#audioMore') && !document.querySelector('#displayMore').open && !document.querySelector('#audioMore').open);
    check('aim-advanced-visible', (document.querySelector('#shapePanel .stepper-field')?.getBoundingClientRect().height || 0) > 0 && (document.querySelector('[data-bind="crosshair.rotation"]')?.getBoundingClientRect().height || 0) > 0);
    check('aim-label-typography', getComputedStyle(document.querySelector('#outlineWidthRow > span')).fontSize === '16px' && getComputedStyle(document.querySelector('#dotSizeRow > span')).fontSize === '16px');
    const steppers = Array.from(document.querySelectorAll('.stepper-field'));
    check('offset-steppers', steppers.length === 2 && steppers.every((field) => field.querySelectorAll('.stepper-buttons button').length === 2));
    const stepButton = document.querySelector('.stepper-buttons button');
    const stepBox = stepButton?.getBoundingClientRect();
    check('offset-stepper-size', !!stepBox && Math.round(stepBox.width) >= 38 && Math.round(stepBox.width) <= 42 && Math.round(stepBox.height) >= 18 && Math.round(stepBox.height) <= 22);
    const offsetX = document.querySelector('[data-bind="crosshair.offset_x"]');
    const beforeOffset = offsetX?.value;
    stepButton?.click();
    check('offset-step-command', String(Number(offsetX?.value)) === String(Number(beforeOffset) + 1));
    stepButton?.click();
    const dotOn = !!document.querySelector('[data-bind="crosshair.dot"]')?.checked;
    check('dot-size-conditional', document.querySelector('#dotSizeRow')?.classList.contains('hidden') === !dotOn);
    const outlineOn = !!document.querySelector('[data-bind="crosshair.outline"]')?.checked;
    check('outline-width-conditional', document.querySelector('#outlineWidthRow')?.classList.contains('hidden') === !outlineOn);
    check('no-style-grid', !document.querySelector('#styleGrid'));
    check('no-placeholder', !document.body.innerText.includes('Initializing in later phase'));
    check('header-controls', ['masterCrosshair', 'masterDisplay', 'masterAudio'].every((id) => document.querySelector('#' + id)));
    const master = document.querySelector('#masterCrosshair');
    master?.click();
    check('master-command', window.__lastSent?.type === 'ToggleMaster');
    master?.click();
    document.querySelector('[data-action="aim-mode"][data-value="image"]')?.click();
    check('image-mode', !document.querySelector('#imagePanel')?.classList.contains('hidden') && document.querySelector('#shapePanel')?.classList.contains('hidden'));
    const solidBefore = !!S.config.crosshair.imageSolid;
    const tintBefore = !!S.config.crosshair.imageTint;
    S.config.crosshair.imageSolid = true;
    S.config.crosshair.imageTint = false;
    syncAimConditionals();
    check('image-solid-row', !document.querySelector('#solidColorRow')?.classList.contains('hidden'));
    check('image-solid-label', document.querySelector('#modeColorLabel')?.textContent === 'Solid color');
    check('image-solid-active', !!document.querySelector('#solidMode [data-action="image-mode"][data-value="solid"]')?.classList.contains('active') && !document.querySelector('#solidMode [data-action="image-mode"][data-value="tint"]')?.classList.contains('active'));
    S.config.crosshair.imageSolid = false;
    S.config.crosshair.imageTint = true;
    syncAimConditionals();
    check('image-tint-row', !document.querySelector('#solidColorRow')?.classList.contains('hidden'));
    check('image-tint-label', document.querySelector('#modeColorLabel')?.textContent === 'Tint color');
    check('image-tint-active', !!document.querySelector('#solidMode [data-action="image-mode"][data-value="tint"]')?.classList.contains('active'));
    S.config.crosshair.imageSolid = solidBefore;
    S.config.crosshair.imageTint = tintBefore;
    syncAimConditionals();
    check('image-mode-restored', document.querySelector('#solidColorRow')?.classList.contains('hidden') === !(solidBefore || tintBefore) && !!document.querySelector('#solidMode [data-action="image-mode"][data-value="original"]')?.classList.contains('active') === !(solidBefore || tintBefore));
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    document.querySelector('[data-tab="display"]')?.click();
    check('display-tab', document.querySelector('#view-display')?.classList.contains('active'));
    check('display-advanced-visible', (document.querySelector('#panelSelect')?.getBoundingClientRect().height || 0) > 0 && (document.querySelector('[data-display="r"]')?.getBoundingClientRect().height || 0) > 0);
    check('display-panels', document.querySelectorAll('#panelSelect option').length === 7);
    const panelBefore = document.querySelector('#panelSelect')?.value;
    applyPreset('display', 'cinema');
    check('display-preset-keeps-panel', document.querySelector('#panelSelect')?.value === panelBefore);
    S.config.display.global.panel = 'auto';
    renderDisplay();
    const autoMonitor = displayMonitor();
    const expectedAutoLabel = `Auto (${String(autoMonitor?.detectedPanel || 'led').toUpperCase()})`;
    check('display-auto-panel-label', document.querySelector('#panelSelect')?.selectedOptions[0]?.textContent === expectedAutoLabel);
    check('display-sliders', document.querySelectorAll('.content > .card [data-display]').length === 4);
    check('display-applied', (document.querySelector('#displayStatus')?.textContent || '') === 'APPLIED');
    check('compare-display-control', (document.querySelector('#compareDisplay')?.getBoundingClientRect().width || 0) > 0);
    const compareControl = document.querySelector('#compareDisplay');
    const compareSlider = document.querySelector('[data-display="brightness"]');
    compareControl?.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, button: 0, pointerId: 7, pointerType: 'mouse', isPrimary: true }));
    check('compare-hold', !!compareControl && S.compareActive === true && compareControl.getAttribute('aria-pressed') === 'true' && !!compareSlider?.disabled && window.__lastSent?.type === 'CompareDisplay' && window.__lastSent?.payload?.active === true);
    compareControl?.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, button: 0, pointerId: 7, pointerType: 'mouse', isPrimary: true }));
    check('compare-release', S.compareActive === false && compareControl?.getAttribute('aria-pressed') === 'false' && !compareSlider?.disabled && window.__lastSent?.type === 'CompareDisplay' && window.__lastSent?.payload?.active === false);
    check('backlight-yellow', getComputedStyle(document.querySelector('#backlightReadout')).color === 'rgb(255, 230, 0)');
    const displayStage = document.querySelector('#displayStage')?.getBoundingClientRect();
    check('display-preview-16-9', !!displayStage && Math.abs(displayStage.width / displayStage.height - 16 / 9) < 0.02);
    check('single-monitor-controls', document.querySelector('#multiMonitorControls')?.classList.contains('hidden'));
    const rgbVars = ['r', 'g', 'b'].map((key) => getComputedStyle(document.querySelector(`[data-display="${key}"]`)).getPropertyValue('--range-color').trim());
    check('rgb-tracks', new Set(rgbVars).size === 3 && rgbVars.every(Boolean));
    document.querySelector('[data-tab="audio"]')?.click();
    check('audio-tab', document.querySelector('#view-audio')?.classList.contains('active'));
    check('audio-advanced-visible', ['audio.echo', 'audio.surround', 'audio.dynamic_boost', 'audio.output_level', 'audio.volume_leveling', 'audio.balance', 'audio.filter_q'].every((path) => { const el = document.querySelector(`[data-bind="${path}"]`); return !!el && el.getBoundingClientRect().height > 0; }));
    check('quick-toggle-module-colors', new Set(['masterCrosshair', 'masterDisplay', 'masterAudio'].map((id) => getComputedStyle(document.querySelector('#' + id)).getPropertyValue('--module').trim())).size === 3);
    check('audio-bands', document.querySelectorAll('#bandGrid .fader').length === 10);
    check('audio-effect-binds', ['audio.bass', 'audio.treble', 'audio.echo', 'audio.surround', 'audio.dynamic_boost', 'audio.output_level', 'audio.volume_leveling', 'audio.balance', 'audio.filter_q'].every((path) => document.querySelector(`[data-bind="${path}"]`)));
    check('audio-master-range', document.querySelector('[data-bind="audio.output_level"]')?.min === '-20' && document.querySelector('[data-bind="audio.output_level"]')?.max === '20');
    check('meter-gradient', getComputedStyle(document.querySelector('#meterFill')).backgroundImage.includes('linear-gradient'));
    check('eq-pchip', typeof window.__crosshairCurveAt === 'function' && Math.abs(window.__crosshairCurveAt({ bands: [0, 0, 6, 0, 0, 0, 0, 0, 0, 0] }, 125) - 6) < 0.01);
    check('eq-edge-smooth', (() => {
      const low = { bands: [6, 0, 0, 0, 0, 0, 0, 0, 0, 0] };
      const high = { bands: [0, 0, 0, 0, 0, 0, 0, 0, 0, 6] };
      return Math.abs(window.__crosshairCurveAt(low, 20) - window.__crosshairCurveAt(low, 21)) > 0.001
        && Math.abs(window.__crosshairCurveAt(high, 16000) - window.__crosshairCurveAt(high, 19990)) > 0.001;
    })());
    check('eq-endpoints-finite', (() => {
      const low = { bands: [6, 0, 0, 0, 0, 0, 0, 0, 0, 0] };
      const high = { bands: [0, 0, 0, 0, 0, 0, 0, 0, 0, 6] };
      return [20, 31, 16000, 20000].every((freq) => {
        const a = window.__crosshairCurveAt(low, freq);
        const b = window.__crosshairCurveAt(high, freq);
        const a2 = window.__crosshairCurveAt(low, freq + 0.001);
        const b2 = window.__crosshairCurveAt(high, freq + 0.001);
        return Number.isFinite(a) && Number.isFinite(b) && Number.isFinite(a2) && Number.isFinite(b2)
          && Math.abs(a - a2) < 0.1 && Math.abs(b - b2) < 0.1;
      });
    })());
    check('eq-stroke', Math.abs(Number(window.__crosshairEqStroke) - 4) < 0.01);
    const bass = document.querySelector('[data-bind="audio.bass"]');
    const beforeBass = Number(bass?.value || 0);
    const testBass = beforeBass >= 10 ? 9 : beforeBass + 1;
    if (bass) {
      bass.value = String(testBass);
      bass.dispatchEvent(new Event('input', { bubbles: true }));
      window.__crosshairFlushAudio();
      check('audio-slider-command', window.__lastSent?.type === 'UpdateSound' && Number(window.__lastSent?.payload?.audio?.bass) === testBass);
      bass.value = String(beforeBass);
      bass.dispatchEvent(new Event('input', { bubbles: true }));
      window.__crosshairFlushAudio();
    } else {
      check('audio-slider-command', false);
    }
    const wideLabels = Array.from(document.querySelectorAll('.fader-label')).map((label) => ({ client: label.clientWidth, scroll: label.scrollWidth }));
    check('audio-labels', wideLabels.every((entry) => entry.scroll <= entry.client + 1));
    check('audio-label-font', getComputedStyle(document.querySelector('.fader-label')).fontSize === '13px');
    check('audio-fader-labels', JSON.stringify(Array.from(document.querySelectorAll('.fader-label')).map((label) => label.textContent)) === JSON.stringify(['Sub', 'Bass', 'Punch', 'Body', 'Warm', 'Voice', 'Clarity', 'Detail', 'Sizzle', 'Air']));
    check('audio-fader-values', Array.from(document.querySelectorAll('.fader-value')).every((value) => getComputedStyle(value).opacity === '1' && /^[+-]?\d+\.\d$/.test(value.textContent.trim())));
    const band0 = document.querySelector('[data-band="0"]');
    const band0Before = band0?.value;
    if (band0) {
      band0.value = '4';
      band0.dispatchEvent(new Event('input', { bubbles: true }));
      const plus = band0.closest('.fader')?.querySelector('.fader-value')?.textContent;
      band0.value = '-3.5';
      band0.dispatchEvent(new Event('input', { bubbles: true }));
      const minus = band0.closest('.fader')?.querySelector('.fader-value')?.textContent;
      check('audio-fader-value-format', plus === '+4.0' && minus === '-3.5' && band0.getAttribute('aria-valuetext') === '-3.5');
      band0.value = band0Before;
      band0.dispatchEvent(new Event('input', { bubbles: true }));
      window.__crosshairFlushAudio?.();
    } else {
      check('audio-fader-value-format', false);
    }
    const main = document.querySelector('#main');
    const mainWidth = main.style.width;
    main.style.width = '1044px';
    const narrowLabels = Array.from(document.querySelectorAll('.fader-label')).map((label) => ({ client: label.clientWidth, scroll: label.scrollWidth }));
    check('audio-labels-narrow', narrowLabels.every((entry) => entry.scroll <= entry.client + 1));
    labelMetrics = { wide: wideLabels, narrow: narrowLabels };
    main.style.width = mainWidth;
    check('no-audio-wall', !document.querySelector('#audioPresets'));
    check('no-inner-audio-switch', !document.querySelector('#view-audio [data-bind="audio.enabled"]'));
    check('audio-save-preset', !!document.querySelector('[data-action="save-preset"][data-module="audio"]'));
    document.querySelector('[data-action="settings"]')?.click();
    check('settings-open', !document.querySelector('#settingsModal')?.classList.contains('hidden'));
    const settingsCard = document.querySelector('#settingsModal .modal-card');
    const settingsCardBox = settingsCard?.getBoundingClientRect();
    check('settings-card-fits', !!settingsCardBox && settingsCardBox.left >= 0 && settingsCardBox.right <= innerWidth + 1 && settingsCardBox.bottom <= innerHeight + 1);
    check('settings-card-complete', !!settingsCard && settingsCard.scrollHeight <= settingsCard.clientHeight + 1);
    check('settings-toggles', document.querySelectorAll('#settingsModal .settings-list .row-toggle').length === 7);
    check('settings-controls', ['setStartWindows', 'setStartMinimized', 'setMinimizeTray', 'setRestoreDisplay', 'setHotkeys', 'setAutoRevert'].every((id) => document.querySelector('#' + id)));
    check('module-hotkey-rows', document.querySelectorAll('#settingsModal .hotkey-row').length === 5 && document.querySelector('#setAimHotkey')?.textContent === 'F8' && !!document.querySelector('#setCycleHotkey'));
    check('settings-hotkey-copy', ['setDisplayHotkey', 'setAudioHotkey', 'setRestoreHotkey', 'setCycleHotkey'].every((id) => document.querySelector('#' + id)?.textContent === 'Set key'));
    const idleHotkey = document.querySelector('#setDisplayHotkey');
    idleHotkey?.dispatchEvent(new FocusEvent('focus'));
    check('settings-hotkey-recording-copy', idleHotkey?.textContent === 'Press key…');
    idleHotkey?.dispatchEvent(new FocusEvent('blur'));
    check('settings-hotkey-idle-copy', idleHotkey?.textContent === 'Set key');
    check('settings-panic-reset', /Panic reset/.test(document.querySelector('#setRestoreHotkey')?.closest('.hotkey-row')?.textContent || '') && /turns Display off/.test(document.querySelector('#setRestoreHotkey')?.closest('.hotkey-row')?.querySelector('span')?.title || ''));
    const aimHotkey = document.querySelector('#setAimHotkey');
    aimHotkey?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', altKey: true, bubbles: true, cancelable: true }));
    check('module-hotkey-capture', window.__lastSent?.type === 'AssignModuleHotkey' && window.__lastSent?.payload?.module === 'aim' && window.__lastSent?.payload?.hotkey === 'Alt+Tab');
    const minimize = document.querySelector('#setMinimizeTray');
    const minimizeBefore = minimize.checked;
    minimize.click();
    document.querySelector('#settingsModal [data-action="save-settings"]')?.click();
    check('settings-command', window.__lastSent?.type === 'SettingsChanged' && window.__lastSent?.payload?.settings?.minimizeToTray === !minimizeBefore);
    document.querySelector('[data-action="settings"]')?.click();
    const maintenanceGrid = document.querySelector('#settingsModal .maintenance-grid');
    const maintenanceButtons = Array.from(maintenanceGrid?.querySelectorAll('.button') || []);
    const maintenanceLabels = maintenanceButtons.map((button) => button.textContent.trim());
    const maintenanceTops = maintenanceButtons.map((button) => Math.round(button.getBoundingClientRect().top));
    check('settings-maintenance', maintenanceLabels.length === 6);
    check('settings-maintenance-grid', !!maintenanceGrid && maintenanceTops.filter((top) => top === maintenanceTops[0]).length === 3 && new Set(maintenanceTops).size === 2);
    check('settings-maintenance-labels', JSON.stringify(maintenanceLabels) === JSON.stringify(['Open FxSound', 'Fix audio', 'Reinstall FxSound', 'App Folder', 'Backups', 'Reset app']));
    check('settings-maintenance-actions', ['open-fxsound', 'unstick-audio', 'repair-audio', 'open-data', 'open-backup', 'factory-reset'].every((action) => maintenanceGrid?.querySelector('[data-action="' + action + '"]')) && !maintenanceGrid?.querySelector('[data-action="open-log"]') && !document.querySelector('#settingsModal [data-action="reset-everything"]') && !document.querySelector('#settingsModal [data-action="erase-settings"]'));
    const versionLabel = document.querySelector('#versionLabel');
    const logLink = document.querySelector('#settingsModal .modal-foot [data-action="open-log"]');
    check('settings-log-link', logLink?.textContent.trim() === 'View logs' && logLink?.previousElementSibling === versionLabel);
    check('settings-save-label', document.querySelector('#settingsModal [data-action="save-settings"]')?.textContent.trim() === 'Save');
    document.querySelector('#settingsModal [data-action="unstick-audio"]')?.click();
    check('unstick-command', window.__lastSent?.type === 'UnstickAudio');
    // Destructive settings actions confirm in the app's own dialog, not the OS one.
    const confirmModal = document.querySelector('#confirmModal');
    window.__crosshairCapture = false;
    document.querySelector('#settingsModal [data-action="factory-reset"]')?.click();
    check('confirm-dialog-open', !!confirmModal && !confirmModal.classList.contains('hidden') && document.querySelector('#confirmTitle')?.textContent === 'Reset Crosshair+?');
    check('confirm-dialog-no-send', window.__lastSent?.type !== 'FactoryReset');
    check('confirm-dialog-danger', document.querySelector('#confirmOk')?.classList.contains('danger'));
    check('confirm-dialog-focus', document.activeElement === document.querySelector('#confirmCancel'));
    document.querySelector('#confirmModal [data-action="confirm-cancel"]')?.click();
    check('confirm-dialog-cancel', !!confirmModal && confirmModal.classList.contains('hidden') && window.__lastSent?.type !== 'FactoryReset');
    confirmAction({ title: 'Self-test confirm', message: 'Body copy', confirm: 'Go' });
    const okOpen = !!confirmModal && !confirmModal.classList.contains('hidden');
    document.querySelector('#confirmOk')?.click();
    check('confirm-dialog-ok', okOpen && window.__crosshairState.confirmOutcome === true && confirmModal.classList.contains('hidden'));
    confirmAction({ title: 'Self-test cancel' });
    document.querySelector('#confirmCancel')?.click();
    check('confirm-dialog-decline', window.__crosshairState.confirmOutcome === false);
    confirmAction({ title: 'Self-test escape' });
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    check('confirm-dialog-escape', window.__crosshairState.confirmOutcome === false && !!confirmModal && confirmModal.classList.contains('hidden'));
    window.__crosshairCapture = true;
    const repairButton = document.querySelector('#settingsModal [data-action="repair-audio"]');
    const fxMessageBefore = document.querySelector('#fxModalMessage')?.textContent;
    check('repair-label', repairButton?.textContent.trim() === 'Reinstall FxSound');
    check('repair-tooltip', /Reinstall FxSound and keep your settings/.test(repairButton?.title || ''));
    repairButton?.click();
    const fxRepairModal = document.querySelector('#fxModal');
    check('repair-modal', !!fxRepairModal && !fxRepairModal.classList.contains('hidden') && document.querySelector('#fxModalTitle')?.textContent === 'Reinstall FxSound' && (document.querySelector('#fxModalMessage')?.textContent || '').includes('will reinstall FxSound') && document.querySelector('#fxInstallButton')?.dataset.action === 'repair-run' && window.__lastSent?.type !== 'RepairAudio');
    closeFxModal();
    check('repair-modal-close', !!fxRepairModal && fxRepairModal.classList.contains('hidden'));
    window.__crosshairState.fxModalMode = 'setup';
    if (fxMessageBefore != null) document.querySelector('#fxModalMessage').textContent = fxMessageBefore;
    check('settings-no-long-help', !document.querySelector('#settingsModal .help-copy'));
    check('settings-short-labels', ['Launch with Windows', 'Start minimized', 'Close to tray', 'Restore screen on exit', 'Keyboard shortcuts', 'Leave slots on close', 'In-game notifications'].every((label) => Array.from(document.querySelectorAll('#settingsModal .settings-list span')).some((span) => span.textContent === label)));
    check('osd-toast-toggle', !!document.querySelector('#setOsdToast'));
    check('blue-light-toggle', !!document.querySelector('#blueLightToggle') && document.querySelector('#blueLightToggle')?.getAttribute('aria-pressed') !== null);
    document.querySelector('#settingsModal [data-action="close-settings"]')?.click();
    check('settings-close', document.querySelector('#settingsModal')?.classList.contains('hidden'));
    document.querySelector('#settingsModal [data-action="open-backup"]')?.click();
    check('backup-modal', !document.querySelector('#backupModal')?.classList.contains('hidden') && window.__lastSent?.type === 'BackupModalOpen');
    check('backup-actions', ['backup-create', 'backup-browse', 'backup-restore', 'backup-undo'].every((action) => document.querySelector('#backupModal [data-action="' + action + '"]')));
    document.querySelector('#backupModal [data-action="close-backup-modal"]')?.click();
    check('backup-close', document.querySelector('#backupModal')?.classList.contains('hidden'));
    document.querySelector('[data-tab="audio"]')?.click();
    check('fx-copy-short', (() => {
      const copy = (document.querySelector('#fxModalMessage')?.textContent || '').trim();
      return !(document.querySelector('#fxModal')?.textContent || '').includes('Audio engine')
        && (/^Install FxSound to control sound here\.$/.test(copy)
          || /^FxSound .* is available\.$/.test(copy)
          || /^FxSound .* needs updating for live control\.$/.test(copy));
    })());
    check('animation-controls', (document.querySelector('#gifOptions .row-toggle span')?.textContent || '') === 'Animate GIF' && !document.body.textContent.includes('GIF playback'));
    check('flatten-eq-action', (document.querySelector('#importStatus [data-action="flatten-eq"]')?.textContent || '').includes('Apply to sliders'));
    document.querySelector('[data-tab="slots"]')?.click();
    const cards = Array.from(document.querySelectorAll('.slot-card'));
    const switchesBefore = Number(window.__crosshairSlotSwitches || 0);
    cards[0]?.click();
    cards[1]?.click();
    check('slot-editor', ((document.querySelector('#slotName')?.value) || '').length > 0);
    check('slot-command', window.__lastSent?.type === 'ActivateSlot');
    check('slot-rapid-single-command', Number(window.__crosshairSlotSwitches || 0) === switchesBefore + 1);
    check('slot-toggle-command', cards[0] && window.__crosshairLastActivation !== undefined);
    check('slot-count', document.querySelectorAll('.slot-card').length === 3);
    check('slot-active-whole-card', (() => { const active = document.querySelector('.slot-card.active'); return !!active && getComputedStyle(active).backgroundColor === 'rgba(182, 146, 255, 0.16)'; })());
    check('no-activate-button', !document.body.innerText.includes('Deactivate') && !/\bActivate\b/.test(document.querySelector('#slotEditor')?.innerText || ''));
    check('custom-dropdowns', document.querySelectorAll('#slotEditor .custom-trigger').length === 4);
    document.querySelector('#slotTarget')?.click();
    check('custom-dropdown-open', document.querySelector('#slotTarget')?.getAttribute('aria-expanded') === 'true' && !document.querySelector('#slotTarget + .custom-menu')?.classList.contains('hidden'));
    check('custom-dropdown-no-scroll', Array.from(document.querySelectorAll('.custom-menu:not(.hidden)')).every((menu) => menu.scrollHeight <= menu.clientHeight + 1));
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    check('menu-escape', document.querySelector('#slotTarget')?.getAttribute('aria-expanded') === 'false' && document.querySelector('#slotTarget + .custom-menu')?.classList.contains('hidden'));
    check('games-entry', !!document.querySelector('[data-action="open-games"]'));
    document.querySelector('[data-action="open-games"]')?.click();
    check('games-modal', !document.querySelector('#gamesModal')?.classList.contains('hidden') && window.__lastSent?.type === 'ScanGames');
    check('games-modal-list', /No games found|Scanning/.test(document.querySelector('#gamesList')?.textContent || ''));
    document.querySelector('#gamesModal [data-action="close-games"]')?.click();
    check('games-modal-close', document.querySelector('#gamesModal')?.classList.contains('hidden'));
    document.querySelector('[data-action="open-games"]')?.click();
    window.__crosshair_onMessage({ v: 1, type: 'InstalledGames', payload: { games: [], timedOut: true } });
    check('games-modal-timeout-copy', /took too long/i.test(document.querySelector('#gamesList')?.textContent || '') && /Rescan/.test(document.querySelector('#gamesList')?.textContent || ''));
    window.__crosshair_onMessage({ v: 1, type: 'InstalledGames', payload: { games: [], timedOut: false } });
    document.querySelector('#gamesModal [data-action="close-games"]')?.click();
    check('no-save-slot', !document.querySelector('[data-action="save-slot"]') && !document.body.innerText.includes('SAVE SLOT'));
    document.querySelector('[data-action="open-emoji"]')?.click();
    check('emoji-picker', document.querySelectorAll('.emoji-button').length === 64);
    check('emoji-grid-size', getComputedStyle(document.querySelector('.emoji-grid')).gridTemplateColumns.split(' ').length === 8 && Math.round(document.querySelector('.emoji-button')?.getBoundingClientRect().height || 0) === 34);
    document.querySelector('#emojiModal [data-action="close-emoji-modal"]')?.click();
    document.querySelector('[data-tab="aim"]')?.click();
    // Neutral bidirectional sliders must look untouched: fill between the
    // slider's own zero mark and the value, not from the left edge.
    const fillProbe = (selector, probeValue) => {
      const input = document.querySelector(selector);
      if (!input) return null;
      const before = input.value;
      input.value = String(probeValue);
      setRangeFill(input);
      const lo = parseFloat(input.style.getPropertyValue("--fill-lo"));
      const hi = parseFloat(input.style.getPropertyValue("--fill-hi"));
      input.value = before;
      setRangeFill(input);
      return [lo, hi];
    };
    const brightnessFill = fillProbe('[data-display="brightness"]', 0);
    check('slider-fill-neutral', !!brightnessFill && brightnessFill[0] === 50 && brightnessFill[1] === 50);
    const contrastFill = fillProbe('[data-display="contrast"]', 0);
    check('slider-fill-zero-offset', !!contrastFill && Math.abs(contrastFill[0] - 42.857142857) < 0.01 && Math.abs(contrastFill[1] - 42.857142857) < 0.01);
    const gainFill = fillProbe('[data-bind="audio.output_level"]', 0);
    check('slider-fill-bipolar', !!gainFill && gainFill[0] === 50 && gainFill[1] === 50);
    check('no-vertical-scroll', document.documentElement.scrollHeight <= innerHeight + 1);
    check('no-pane-scroll', Array.from(document.querySelectorAll('.preset-menu, .custom-menu, .emoji-grid, .modal-card')).every((el) => el.classList.contains('hidden') || el.scrollHeight <= el.clientHeight + 1));
    // A load-time EQ cleanup banner has to reach the user as a visible toast; the
    // Rust test only proves the event is emitted, not that the page displays it.
    const toast = document.querySelector('#toast');
    const cleanupText = "The sound profile 'Hand edited' was not applied because its preamp is not a valid number.";
    window.__crosshair_onMessage({ v: 1, type: 'DiagnosticBanner', payload: { code: 'eq', message: cleanupText, recoverable: true } });
    check('eq-cleanup-toast', !!toast && !toast.classList.contains('hidden') && toast.textContent === cleanupText);
    check('eq-cleanup-toast-error', !!toast && toast.classList.contains('error'));
    const toastBox = toast.getBoundingClientRect();
    const settingsBox = document.querySelector('.statusbar-settings')?.getBoundingClientRect();
    const tabsRect = document.querySelector('.tabs')?.getBoundingClientRect();
    const topbarRect = document.querySelector('.topbar')?.getBoundingClientRect();
    check('toast-in-tab-row', !!topbarRect && toastBox.top >= topbarRect.top - 1 && toastBox.bottom <= topbarRect.bottom + 1 && Math.round(innerWidth - toastBox.right) === 24);
    check('toast-clear-of-tabs', !!tabsRect && (toastBox.left >= tabsRect.right || toastBox.right <= tabsRect.left || toastBox.bottom <= tabsRect.top || toastBox.top >= tabsRect.bottom));
    check('toast-clear-of-settings', !!settingsBox && (toastBox.bottom <= settingsBox.top || toastBox.right <= settingsBox.left));
    check('toast-full-text-title', toast.title === cleanupText);
    window.__crosshair_onMessage({ v: 1, type: 'DiagnosticBanner', payload: { code: 'audio_notice', message: 'FxSound reinstalled.', recoverable: true } });
    check('audio-notice-success-toast', !toast.classList.contains('error') && toast.textContent === 'FxSound reinstalled.');
    window.__crosshair_onMessage({ v: 1, type: 'DiagnosticBanner', payload: { code: 'audio_notice', message: 'Reinstall failed.', recoverable: true } });
    check('audio-notice-error-toast', toast.classList.contains('error'));
    check('eq-cleanup-toast-single', document.querySelectorAll('.toast').length === 1);
    // Reporting the same cleanup twice refreshes the one toast instead of stacking.
    window.__crosshair_onMessage({ v: 1, type: 'DiagnosticBanner', payload: { code: 'eq', message: cleanupText, recoverable: true } });
    check('eq-cleanup-toast-no-stack', document.querySelectorAll('.toast').length === 1 && !!toast && toast.textContent === cleanupText && !toast.classList.contains('hidden'));
    toast.classList.add('hidden');
    document.querySelector('#settingsModal [data-action="close-settings"]')?.click();
    const contentFits = (tab) => {
      window.__crosshair_setTab(tab);
      const content = document.querySelector('#view-' + tab + ' .content');
      return !!content && content.scrollHeight <= content.clientHeight + 1;
    };
    // A loaded image shows the option stack; a GIF plus tint/solid shows the
    // tallest variant. Force every conditional block visible for the measure so
    // an empty dropzone can never hide an overflow again.
    const imageWorstCaseFits = () => {
      const forced = ['#imageOptions', '#solidColorRow', '#gifOptions'].map((selector) => document.querySelector(selector));
      const classes = forced.map((el) => (el ? el.className : null));
      forced.forEach((el) => el?.classList.remove('hidden'));
      const fits = contentFits('aim');
      forced.forEach((el, index) => { if (el) el.className = classes[index]; });
      return fits;
    };
    window.__crosshair_setTab('aim');
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    check('no-scroll-aim-shape', contentFits('aim'));
    document.querySelector('[data-action="aim-mode"][data-value="image"]')?.click();
    check('no-scroll-aim-image', contentFits('aim'));
    check('no-scroll-aim-image-loaded', imageWorstCaseFits());
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    window.__crosshair_setTab('display');
    const displayMore = document.querySelector('#displayMore');
    if (displayMore) displayMore.open = false;
    check('no-scroll-display-collapsed', contentFits('display'));
    if (displayMore) displayMore.open = true;
    check('no-scroll-display-expanded', contentFits('display'));
    if (displayMore) displayMore.open = false;
    window.__crosshair_setTab('audio');
    const audioMore = document.querySelector('#audioMore');
    if (audioMore) audioMore.open = false;
    check('no-scroll-audio-collapsed', contentFits('audio'));
    if (audioMore) audioMore.open = true;
    check('no-scroll-audio-expanded', contentFits('audio'));
    if (audioMore) audioMore.open = false;
    check('no-scroll-slots', contentFits('slots'));
    // Recent-image selection is confirmed by the backend before the image tab
    // is shown; a vanished file refreshes the list instead of leaving a dead row.
    const recentImagesBackup = S.crosshairImages;
    const tabBackup = document.body.dataset.tab;
    window.__crosshair_setTab('aim');
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    const fakeRecentName = '9999999999999-gone.png';
    const fakeRecent = [{ name: fakeRecentName, label: 'gone.png' }];
    // Mirrors the backend's FullState payload; live values are reused so only
    // the Recent list changes during the check.
    const recentFullState = (images) => ({
      config: S.config,
      monitors: S.monitors || [],
      fxSound: S.fx || {},
      audioDevices: S.devices || [],
      crosshairPresets: S.crosshairPresets || [],
      displayPresets: S.displayPresets || [],
      audioPresets: S.audioPresets || [],
      appVersion: S.appVersion || '1.0.0',
      selectedSlot: S.selectedSlot || null,
      runningApps: S.runningApps || [],
      installedGames: S.installedGames || [],
      canUndoRestore: !!S.canUndoRestore,
      crosshairImages: images,
    });
    const openRecent = () => {
      document.querySelector('#recentTrigger')?.click();
      // The click asks for a disk scan; answer it with the fake row so the menu
      // opens on fresh data exactly as it does in the app.
      window.__crosshair_onMessage({ v: 1, type: 'RecentImages', payload: { images: fakeRecent } });
      document.querySelector('#recentMenu .recent-pick')?.click();
    };
    // Real temp-folder flow used by --capture. The Rust harness seeds the
    // sandbox crosshairs folder (recent-alive.webp) and deletes the dead file
    // from disk. This injects the stale cache the app could still hold, then
    // opens Recent; no row may be painted before the backend's fresh scan
    // answers. The wrapper finalizes on that answer instead of polling.
    window.__crosshairRecentStart = () => {
      const dead = 'recent-dead.webp';
      const alive = 'recent-alive.webp';
      const stale = [{ name: dead, label: 'dead.webp' }, { name: alive, label: 'alive.webp' }];
      const menu = document.querySelector('#recentMenu');
      const trigger = document.querySelector('#recentTrigger');
      document.body.click();
      window.__crosshair_onMessage({ v: 1, type: 'RecentImages', payload: { images: stale } });
      const cacheKnown = (S.crosshairImages || []).length === stale.length;
      trigger?.click();
      const pre = {
        cacheKnown,
        hidden: !!menu?.classList.contains('hidden'),
        rows: menu?.querySelectorAll('.recent-pick')?.length || 0,
        loading: !!S.recentLoading,
        sent: window.__lastSent?.type,
      };
      const errors = [];
      if (!cacheKnown) errors.push('cache-not-seeded');
      if (!pre.hidden || pre.rows !== 0 || !pre.loading || pre.sent !== 'RefreshRecentImages') {
        errors.push('cached-paint-before-scan');
      }
      const probe = { ok: false, errors, pre, rows: [], names: [], menuHidden: null };
      window.__crosshairRecentProbe = probe;
      const original = window.__crosshair_onMessage;
      let settled = false;
      // The app handles the scan synchronously; finalizing after it returns
      // reads the settled menu instead of racing a timer.
      const finish = () => {
        if (settled) return;
        settled = true;
        window.__crosshair_onMessage = original;
        const current = document.querySelector('#recentMenu');
        probe.rows = Array.from(current?.querySelectorAll('.recent-pick') || []).map((el) => el.dataset.name);
        probe.names = (S.crosshairImages || []).map((image) => image.name);
        probe.menuHidden = !!current?.classList.contains('hidden');
        if (probe.rows.length !== 1) probe.errors.push('fresh-row-count');
        if (probe.rows.includes(dead) || probe.names.includes(dead)) probe.errors.push('deleted-row');
        if (!probe.rows.includes(alive)) probe.errors.push('fresh-row-missing');
        if (probe.menuHidden) probe.errors.push('menu-not-open');
        if (S.recentLoading) probe.errors.push('still-loading');
        probe.ok = probe.errors.length === 0;
      };
      window.__crosshair_onMessage = (message) => {
        original(message);
        if (message?.type === 'RecentImages') finish();
      };
      return errors;
    };
    openRecent();
    check('recent-confirm-row-visible', !document.querySelector('#recentImages')?.classList.contains('hidden'));
    check('recent-confirm-pending', S.aimMode === 'shape' && S.pendingStoredImage === fakeRecentName);
    // Real failure order: the refreshed state arrives before the failure result,
    // so the dead row disappears while the pick is still pending and the tab
    // still has not moved.
    window.__crosshair_onMessage({ v: 1, type: 'FullState', payload: recentFullState([]) });
    check('recent-confirm-failure-refreshes-first', S.crosshairImages.length === 0
      && document.querySelector('#recentImages')?.classList.contains('hidden')
      && S.aimMode === 'shape' && S.pendingStoredImage === fakeRecentName);
    window.__crosshair_onMessage({ v: 1, type: 'StoredImageResult', payload: { name: fakeRecentName, ok: false } });
    check('recent-confirm-failure-keeps-shape', S.aimMode === 'shape' && S.pendingStoredImage === null);
    openRecent();
    check('recent-confirm-success-pending', S.aimMode === 'shape' && S.pendingStoredImage === fakeRecentName);
    window.__crosshair_onMessage({ v: 1, type: 'StoredImageResult', payload: { name: '9999999999998-other.png', ok: true } });
    check('recent-confirm-stale-result-ignored', S.aimMode === 'shape' && S.pendingStoredImage === fakeRecentName);
    window.__crosshair_onMessage({ v: 1, type: 'FullState', payload: recentFullState(fakeRecent) });
    check('recent-confirm-success-waits', S.aimMode === 'shape' && document.querySelector('#imagePanel')?.classList.contains('hidden'));
    window.__crosshair_onMessage({ v: 1, type: 'StoredImageResult', payload: { name: fakeRecentName, ok: true } });
    check('recent-confirm-success-switches', S.aimMode === 'image' && S.pendingStoredImage === null && !document.querySelector('#imagePanel')?.classList.contains('hidden'));
    S.crosshairImages = recentImagesBackup;
    document.querySelector('[data-action="aim-mode"][data-value="shape"]')?.click();
    window.__crosshair_setTab(tabBackup || 'aim');
    // Capture-only: the harness seeded/deleted the files above. Open Recent
    // with the deleted name still in the cached list; the sync check fails if
    // any row was painted before the backend's disk scan answered. The deferred
    // probe records the menu after that answer (recent-menu).
    window.__crosshair_setTab('aim');
    const realRecentErrors = window.__crosshairRecentStart() || [];
    check('recent-real-no-cached-paint', realRecentErrors.length === 0);
  } catch (error) {
    errors.push('exception:' + String(error));
  }
  return JSON.stringify({ ok: errors.length === 0, errors, width: innerWidth, height: innerHeight, dpr: devicePixelRatio, previewWidth: document.querySelector('#aimPreview')?.naturalWidth || 0, labelMetrics });
})()