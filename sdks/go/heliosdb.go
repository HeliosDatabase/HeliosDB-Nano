// Package heliosdb provides a Go client for HeliosDB-Lite.
//
// Example usage:
//
//	client, err := heliosdb.NewClient("http://localhost:8080",
//	    heliosdb.WithAPIKey("your-api-key"),
//	    heliosdb.WithBranch("main"),
//	)
//	if err != nil {
//	    log.Fatal(err)
//	}
//
//	// Execute SQL query
//	result, err := client.Query(ctx, "SELECT * FROM users WHERE id = $1", 1)
//
//	// Vector search
//	results, err := client.VectorSearch(ctx, "documents", "hello world",
//	    heliosdb.WithTopK(10),
//	    heliosdb.WithMinScore(0.7),
//	)
package heliosdb

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"time"
)

// Client is the HeliosDB client.
type Client struct {
	baseURL    string
	apiKey     string
	branch     string
	httpClient *http.Client
}

// ClientOption configures the client.
type ClientOption func(*Client)

// WithAPIKey sets the API key for authentication.
func WithAPIKey(key string) ClientOption {
	return func(c *Client) {
		c.apiKey = key
	}
}

// WithBranch sets the default branch.
func WithBranch(branch string) ClientOption {
	return func(c *Client) {
		c.branch = branch
	}
}

// WithHTTPClient sets a custom HTTP client.
func WithHTTPClient(client *http.Client) ClientOption {
	return func(c *Client) {
		c.httpClient = client
	}
}

// WithTimeout sets the default timeout.
func WithTimeout(d time.Duration) ClientOption {
	return func(c *Client) {
		c.httpClient.Timeout = d
	}
}

// NewClient creates a new HeliosDB client.
func NewClient(baseURL string, opts ...ClientOption) (*Client, error) {
	if _, err := url.Parse(baseURL); err != nil {
		return nil, fmt.Errorf("invalid base URL: %w", err)
	}

	c := &Client{
		baseURL: baseURL,
		branch:  "main",
		httpClient: &http.Client{
			Timeout: 30 * time.Second,
		},
	}

	for _, opt := range opts {
		opt(c)
	}

	return c, nil
}

// QueryResult represents the result of a SQL query.
type QueryResult struct {
	Rows        []map[string]interface{} `json:"rows"`
	Columns     []string                 `json:"columns"`
	RowsAffected int64                   `json:"rows_affected,omitempty"`
}

// Query executes a SQL query.
func (c *Client) Query(ctx context.Context, sql string, params ...interface{}) (*QueryResult, error) {
	return c.QueryOnBranch(ctx, c.branch, sql, params...)
}

// QueryOnBranch executes a SQL query on a specific branch.
func (c *Client) QueryOnBranch(ctx context.Context, branch, sql string, params ...interface{}) (*QueryResult, error) {
	body := map[string]interface{}{
		"sql":    sql,
		"params": params,
	}

	var result QueryResult
	err := c.post(ctx, fmt.Sprintf("/v1/branches/%s/query", branch), body, &result)
	if err != nil {
		return nil, err
	}

	return &result, nil
}

// Exec executes a SQL statement that doesn't return rows.
func (c *Client) Exec(ctx context.Context, sql string, params ...interface{}) (int64, error) {
	result, err := c.Query(ctx, sql, params...)
	if err != nil {
		return 0, err
	}
	return result.RowsAffected, nil
}

// VectorStore represents a vector store.
type VectorStore struct {
	Name       string `json:"name"`
	Dimensions int    `json:"dimensions"`
	Metric     string `json:"metric"`
	Count      int64  `json:"count"`
}

// VectorSearchResult represents a single search result.
type VectorSearchResult struct {
	ID       string                 `json:"id"`
	Score    float64                `json:"score"`
	Content  string                 `json:"content,omitempty"`
	Metadata map[string]interface{} `json:"metadata,omitempty"`
}

// VectorSearchOptions configures vector search.
type VectorSearchOptions struct {
	TopK     int
	MinScore float64
	Filter   map[string]interface{}
}

// VectorSearchOption configures vector search.
type VectorSearchOption func(*VectorSearchOptions)

// WithTopK sets the maximum number of results.
func WithTopK(k int) VectorSearchOption {
	return func(o *VectorSearchOptions) {
		o.TopK = k
	}
}

// WithMinScore sets the minimum similarity score.
func WithMinScore(score float64) VectorSearchOption {
	return func(o *VectorSearchOptions) {
		o.MinScore = score
	}
}

// WithFilter sets metadata filter.
func WithFilter(filter map[string]interface{}) VectorSearchOption {
	return func(o *VectorSearchOptions) {
		o.Filter = filter
	}
}

// VectorSearch performs semantic search by text.
func (c *Client) VectorSearch(ctx context.Context, store, query string, opts ...VectorSearchOption) ([]VectorSearchResult, error) {
	options := &VectorSearchOptions{
		TopK: 10,
	}
	for _, opt := range opts {
		opt(options)
	}

	body := map[string]interface{}{
		"text":   query,
		"top_k":  options.TopK,
	}
	if options.MinScore > 0 {
		body["min_score"] = options.MinScore
	}
	if options.Filter != nil {
		body["filter"] = options.Filter
	}

	var response struct {
		Results []VectorSearchResult `json:"results"`
	}

	err := c.post(ctx, fmt.Sprintf("/v1/vectors/stores/%s/search/text", store), body, &response)
	if err != nil {
		return nil, err
	}

	return response.Results, nil
}

// VectorSearchByVector performs search by vector.
func (c *Client) VectorSearchByVector(ctx context.Context, store string, vector []float32, opts ...VectorSearchOption) ([]VectorSearchResult, error) {
	options := &VectorSearchOptions{
		TopK: 10,
	}
	for _, opt := range opts {
		opt(options)
	}

	body := map[string]interface{}{
		"vector": vector,
		"top_k":  options.TopK,
	}
	if options.MinScore > 0 {
		body["min_score"] = options.MinScore
	}
	if options.Filter != nil {
		body["filter"] = options.Filter
	}

	var response struct {
		Results []VectorSearchResult `json:"results"`
	}

	err := c.post(ctx, fmt.Sprintf("/v1/vectors/stores/%s/search", store), body, &response)
	if err != nil {
		return nil, err
	}

	return response.Results, nil
}

// StoreText stores text with automatic embedding.
func (c *Client) StoreText(ctx context.Context, store, text string, metadata map[string]interface{}) (string, error) {
	body := map[string]interface{}{
		"texts":     []string{text},
		"metadatas": []map[string]interface{}{metadata},
	}

	var response struct {
		IDs []string `json:"ids"`
	}

	err := c.post(ctx, fmt.Sprintf("/v1/vectors/stores/%s/texts", store), body, &response)
	if err != nil {
		return "", err
	}

	if len(response.IDs) > 0 {
		return response.IDs[0], nil
	}
	return "", nil
}

// Branch represents a database branch.
type Branch struct {
	Name      string  `json:"name"`
	Parent    *string `json:"parent,omitempty"`
	CreatedAt string  `json:"created_at"`
}

// ListBranches returns all branches.
func (c *Client) ListBranches(ctx context.Context) ([]Branch, error) {
	var response struct {
		Branches []Branch `json:"branches"`
	}

	err := c.get(ctx, "/v1/branches", &response)
	if err != nil {
		return nil, err
	}

	return response.Branches, nil
}

// CreateBranch creates a new branch.
func (c *Client) CreateBranch(ctx context.Context, name, fromBranch string) (*Branch, error) {
	body := map[string]interface{}{
		"name":        name,
		"from_branch": fromBranch,
	}

	var branch Branch
	err := c.post(ctx, "/v1/branches", body, &branch)
	if err != nil {
		return nil, err
	}

	return &branch, nil
}

// MergeBranch merges a branch into another.
func (c *Client) MergeBranch(ctx context.Context, source, target string) error {
	body := map[string]interface{}{
		"target": target,
	}

	return c.post(ctx, fmt.Sprintf("/v1/branches/%s/merge", source), body, nil)
}

// SetBranch sets the default branch for queries.
func (c *Client) SetBranch(branch string) {
	c.branch = branch
}

// GetBranch returns the current default branch.
func (c *Client) GetBranch() string {
	return c.branch
}

// AgentMemory provides agent memory operations.
type AgentMemory struct {
	client    *Client
	sessionID string
}

// Memory returns an agent memory instance for the given session.
func (c *Client) Memory(sessionID string) *AgentMemory {
	return &AgentMemory{
		client:    c,
		sessionID: sessionID,
	}
}

// Message represents a chat message.
type Message struct {
	Role      string `json:"role"`
	Content   string `json:"content"`
	Timestamp string `json:"timestamp,omitempty"`
}

// Add adds a message to memory.
func (m *AgentMemory) Add(ctx context.Context, role, content string) error {
	body := map[string]interface{}{
		"role":    role,
		"content": content,
	}

	return m.client.post(ctx, fmt.Sprintf("/v1/agents/memory/%s/add", m.sessionID), body, nil)
}

// Get retrieves messages from memory.
func (m *AgentMemory) Get(ctx context.Context, limit int) ([]Message, error) {
	var response struct {
		Messages []Message `json:"messages"`
	}

	err := m.client.get(ctx, fmt.Sprintf("/v1/agents/memory/%s/messages?limit=%d", m.sessionID, limit), &response)
	if err != nil {
		return nil, err
	}

	return response.Messages, nil
}

// Search searches memory semantically.
func (m *AgentMemory) Search(ctx context.Context, query string, topK int) ([]VectorSearchResult, error) {
	body := map[string]interface{}{
		"query": query,
		"top_k": topK,
	}

	var response struct {
		Results []VectorSearchResult `json:"results"`
	}

	err := m.client.post(ctx, fmt.Sprintf("/v1/agents/memory/%s/search", m.sessionID), body, &response)
	if err != nil {
		return nil, err
	}

	return response.Results, nil
}

// Clear clears the memory.
func (m *AgentMemory) Clear(ctx context.Context) error {
	return m.client.delete(ctx, fmt.Sprintf("/v1/agents/memory/%s", m.sessionID))
}

// QueryAt queries data at a specific point in time.
func (c *Client) QueryAt(ctx context.Context, sql string, timestamp time.Time, params ...interface{}) (*QueryResult, error) {
	body := map[string]interface{}{
		"sql":             sql,
		"params":          params,
		"as_of_timestamp": timestamp.Format(time.RFC3339),
	}

	var result QueryResult
	err := c.post(ctx, fmt.Sprintf("/v1/branches/%s/query", c.branch), body, &result)
	if err != nil {
		return nil, err
	}

	return &result, nil
}

// NLQuery executes a natural language query.
func (c *Client) NLQuery(ctx context.Context, question string) (*QueryResult, string, error) {
	body := map[string]interface{}{
		"question": question,
		"branch":   c.branch,
	}

	var response struct {
		SQL    string                   `json:"sql"`
		Rows   []map[string]interface{} `json:"rows"`
		Columns []string                `json:"columns"`
	}

	err := c.post(ctx, "/v1/nl/query", body, &response)
	if err != nil {
		return nil, "", err
	}

	result := &QueryResult{
		Rows:    response.Rows,
		Columns: response.Columns,
	}

	return result, response.SQL, nil
}

// Health checks the server health.
func (c *Client) Health(ctx context.Context) error {
	return c.get(ctx, "/health", nil)
}

// HTTP helpers

func (c *Client) get(ctx context.Context, path string, result interface{}) error {
	req, err := http.NewRequestWithContext(ctx, "GET", c.baseURL+path, nil)
	if err != nil {
		return err
	}

	return c.do(req, result)
}

func (c *Client) post(ctx context.Context, path string, body, result interface{}) error {
	jsonBody, err := json.Marshal(body)
	if err != nil {
		return err
	}

	req, err := http.NewRequestWithContext(ctx, "POST", c.baseURL+path, bytes.NewReader(jsonBody))
	if err != nil {
		return err
	}

	return c.do(req, result)
}

func (c *Client) delete(ctx context.Context, path string) error {
	req, err := http.NewRequestWithContext(ctx, "DELETE", c.baseURL+path, nil)
	if err != nil {
		return err
	}

	return c.do(req, nil)
}

func (c *Client) do(req *http.Request, result interface{}) error {
	req.Header.Set("Content-Type", "application/json")
	if c.apiKey != "" {
		req.Header.Set("X-API-Key", c.apiKey)
	}

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode >= 400 {
		body, _ := io.ReadAll(resp.Body)
		return &APIError{
			StatusCode: resp.StatusCode,
			Message:    string(body),
		}
	}

	if result != nil {
		return json.NewDecoder(resp.Body).Decode(result)
	}

	return nil
}

// APIError represents an API error.
type APIError struct {
	StatusCode int
	Message    string
}

func (e *APIError) Error() string {
	return fmt.Sprintf("API error %d: %s", e.StatusCode, e.Message)
}
