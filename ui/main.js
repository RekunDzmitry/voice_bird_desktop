const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// State
let availableSessions = [];
let selectedSessions = new Set();
let activeSessions = new Map();
let currentView = 'api-key'; // 'api-key' | 'browser' | 'recording'

// DOM Elements
const apiKeySection = document.getElementById('api-key-section');
const sessionBrowser = document.getElementById('session-browser');
const recordingDashboard = document.getElementById('recording-dashboard');
const sessionList = document.getElementById('session-list');
const activeSessContainer = document.getElementById('active-sessions');
const serverStatus = document.getElementById('server-status');
const startBtn = document.getElementById('start-btn');
const refreshBtn = document.getElementById('refresh-btn');
const stopAllBtn = document.getElementById('stop-all-btn');
const settingsBtn = document.getElementById('settings-btn');
const statusMessage = document.getElementById('status-message');
const apiKeyInput = document.getElementById('api-key-input');
const toggleVisibilityBtn = document.getElementById('toggle-visibility');
const saveKeyBtn = document.getElementById('save-key-btn');
const backBtn = document.getElementById('back-btn');

// Initialize
async function init() {
    setupEventListeners();
    setupTauriListeners();

    // Check if API key is configured
    const [configured, url] = await invoke('get_api_key_status');

    if (configured) {
        serverStatus.textContent = 'Connected';
        serverStatus.className = 'status-indicator connected';
        setStatus(`Server: ${url}`);
        showSessionBrowser();
        await refreshSessions();
    } else {
        serverStatus.textContent = 'Setup';
        serverStatus.className = 'status-indicator disconnected';
        setStatus('Enter your API key to get started');
        showApiKeySection();
    }
}

// === View Management ===

function showApiKeySection(canGoBack = false) {
    currentView = 'api-key';
    apiKeySection.classList.remove('hidden');
    sessionBrowser.classList.add('hidden');
    recordingDashboard.classList.add('hidden');
    apiKeyInput.value = '';
    apiKeyInput.focus();

    // Show back button only if user can return (already configured)
    if (canGoBack) {
        backBtn.classList.remove('hidden');
    } else {
        backBtn.classList.add('hidden');
    }
}

function showSessionBrowser() {
    currentView = 'browser';
    apiKeySection.classList.add('hidden');
    sessionBrowser.classList.remove('hidden');
    recordingDashboard.classList.add('hidden');
    selectedSessions.clear();
    updateStartButton();
}

function showRecordingView() {
    currentView = 'recording';
    apiKeySection.classList.add('hidden');
    sessionBrowser.classList.add('hidden');
    recordingDashboard.classList.remove('hidden');
    renderActiveSessions();
}

// === API Key Management ===

async function saveApiKey() {
    const apiKey = apiKeyInput.value.trim();
    if (!apiKey) {
        setStatus('Please enter an API key');
        return;
    }

    saveKeyBtn.disabled = true;
    saveKeyBtn.textContent = 'Saving...';

    try {
        await invoke('save_api_key', { apiKey });

        serverStatus.textContent = 'Connected';
        serverStatus.className = 'status-indicator connected';

        const [, url] = await invoke('get_api_key_status');
        setStatus(`Connected to ${url}`);

        showSessionBrowser();
        await refreshSessions();
    } catch (e) {
        console.error('Failed to save API key:', e);
        setStatus('Failed to save API key: ' + e);
    }

    saveKeyBtn.disabled = false;
    saveKeyBtn.textContent = 'Save API Key';
}

function toggleApiKeyVisibility() {
    if (apiKeyInput.type === 'password') {
        apiKeyInput.type = 'text';
        toggleVisibilityBtn.textContent = 'Hide';
    } else {
        apiKeyInput.type = 'password';
        toggleVisibilityBtn.textContent = 'Show';
    }
}

function updateSaveButton() {
    saveKeyBtn.disabled = apiKeyInput.value.trim().length === 0;
}

async function goBackFromSettings() {
    showSessionBrowser();
    await refreshSessions();
}

// === Session Management ===

async function refreshSessions() {
    sessionList.innerHTML = '<p class="loading">Loading sessions...</p>';

    try {
        availableSessions = await invoke('enumerate_sessions');
        selectedSessions.clear();
        renderSessionList();
    } catch (e) {
        console.error('Failed to enumerate sessions:', e);
        sessionList.innerHTML = '<p class="empty">Failed to enumerate sessions. Make sure applications are playing audio.</p>';
    }
}

function renderSessionList() {
    sessionList.innerHTML = '';

    if (availableSessions.length === 0) {
        sessionList.innerHTML = '<p class="empty">No active audio sessions found. Make sure applications are playing or recording audio.</p>';
        return;
    }

    availableSessions.forEach((session, index) => {
        const item = document.createElement('div');
        item.className = 'session-item' + (selectedSessions.has(index) ? ' selected' : '');
        item.innerHTML = `
            <input type="checkbox" class="session-checkbox"
                   ${selectedSessions.has(index) ? 'checked' : ''}>
            <div class="session-info">
                <div class="session-name">${escapeHtml(session.app_name)}</div>
                <div class="session-device">${escapeHtml(session.device_name)}</div>
            </div>
            <span class="session-type ${session.is_input ? 'input' : 'output'}">${session.is_input ? 'Input' : 'Output'}</span>
        `;

        item.addEventListener('click', (e) => {
            if (e.target.type !== 'checkbox') {
                toggleSession(index);
            }
        });

        const checkbox = item.querySelector('.session-checkbox');
        checkbox.addEventListener('change', () => toggleSession(index));

        sessionList.appendChild(item);
    });

    updateStartButton();
}

function toggleSession(index) {
    if (selectedSessions.has(index)) {
        selectedSessions.delete(index);
    } else {
        selectedSessions.add(index);
    }
    renderSessionList();
}

function updateStartButton() {
    const count = selectedSessions.size;
    startBtn.disabled = count === 0;
    startBtn.textContent = count > 0 ? `Start Recording (${count})` : 'Start Recording';
}

// === Recording ===

async function startRecording() {
    const sessionsToRecord = Array.from(selectedSessions).map(i => availableSessions[i]);

    if (sessionsToRecord.length === 0) {
        setStatus('No sessions selected');
        return;
    }

    startBtn.disabled = true;
    startBtn.textContent = 'Starting...';

    try {
        const sessionIds = await invoke('start_recording', { sessions: sessionsToRecord });

        sessionIds.forEach((id, i) => {
            const sessionInfo = sessionsToRecord[i];
            if (sessionInfo) {
                activeSessions.set(id, {
                    ...sessionInfo,
                    id,
                    level: 0,
                    duration: 0,
                    status: 'Recording'
                });
            }
        });

        showRecordingView();
        setStatus(`Recording ${sessionIds.length} session(s)`);
    } catch (e) {
        console.error('Failed to start recording:', e);
        setStatus('Failed to start recording: ' + e);
        startBtn.disabled = false;
        startBtn.textContent = 'Start Recording';
    }
}

async function stopAllRecording() {
    stopAllBtn.disabled = true;
    stopAllBtn.textContent = 'Stopping...';

    try {
        await invoke('stop_all_sessions');
        activeSessions.clear();
        setStatus('Recording stopped');
        showSessionBrowser();
        await refreshSessions();
    } catch (e) {
        console.error('Failed to stop recording:', e);
        setStatus('Failed to stop: ' + e);
    }

    stopAllBtn.disabled = false;
    stopAllBtn.textContent = 'Stop All';
}

// === Active Sessions Display ===

function renderActiveSessions() {
    activeSessContainer.innerHTML = '';

    if (activeSessions.size === 0) {
        activeSessContainer.innerHTML = '<p class="empty">No active sessions</p>';
        return;
    }

    activeSessions.forEach((session, id) => {
        const card = document.createElement('div');
        card.className = 'active-session';
        card.id = `session-${id}`;
        card.innerHTML = `
            <div class="active-session-header">
                <div class="active-session-info">
                    <strong>${escapeHtml(session.app_name)}</strong>
                    <div class="session-device">${escapeHtml(session.device_name)}</div>
                </div>
                <div class="recording-indicator">
                    <span class="recording-dot"></span>
                    <span>Recording</span>
                </div>
            </div>
            <div class="audio-meter">
                <div class="audio-meter-fill" style="width: 0%"></div>
            </div>
            <div class="duration">0:00</div>
        `;
        activeSessContainer.appendChild(card);
    });
}

function updateAudioLevel(sessionId, level) {
    const card = document.getElementById(`session-${sessionId}`);
    if (!card) return;

    const meter = card.querySelector('.audio-meter-fill');
    const percentage = Math.min(level * 100 * 3, 100); // Amplify for visibility

    meter.style.width = percentage + '%';

    meter.className = 'audio-meter-fill';
    if (percentage > 70) {
        meter.classList.add('high');
    } else if (percentage > 40) {
        meter.classList.add('medium');
    }

    const session = activeSessions.get(sessionId);
    if (session) {
        session.level = level;
    }
}

function updateDuration(sessionId, duration) {
    const card = document.getElementById(`session-${sessionId}`);
    if (!card) return;

    const durationEl = card.querySelector('.duration');
    durationEl.textContent = formatDuration(duration);
}

// === Utilities ===

function formatDuration(seconds) {
    const mins = Math.floor(seconds / 60);
    const secs = Math.floor(seconds % 60);
    return `${mins}:${secs.toString().padStart(2, '0')}`;
}

function setStatus(message) {
    statusMessage.textContent = message;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text || '';
    return div.innerHTML;
}

// === Event Listeners ===

function setupEventListeners() {
    // API Key section
    apiKeyInput.addEventListener('input', updateSaveButton);
    apiKeyInput.addEventListener('keypress', (e) => {
        if (e.key === 'Enter' && !saveKeyBtn.disabled) {
            saveApiKey();
        }
    });
    toggleVisibilityBtn.addEventListener('click', toggleApiKeyVisibility);
    saveKeyBtn.addEventListener('click', saveApiKey);

    // Session browser
    refreshBtn.addEventListener('click', refreshSessions);
    startBtn.addEventListener('click', startRecording);
    settingsBtn.addEventListener('click', () => showApiKeySection(true));
    backBtn.addEventListener('click', goBackFromSettings);

    // Recording dashboard
    stopAllBtn.addEventListener('click', stopAllRecording);
}

async function setupTauriListeners() {
    await listen('audio-level', (event) => {
        const { session_id, level } = event.payload;
        updateAudioLevel(session_id, level);
    });
}

// Poll for session states (for duration updates)
setInterval(async () => {
    if (currentView !== 'recording' || activeSessions.size === 0) return;

    try {
        const states = await invoke('get_sessions_state');
        states.forEach(state => {
            updateDuration(state.id, state.duration);
        });
    } catch (e) {
        console.error('Failed to get session states:', e);
    }
}, 1000);

// Initialize app when DOM is ready
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
} else {
    init();
}
